use super::*;
use audio::AudioError;
use injection::ProvisionalInsertion;
use tokio::sync::watch;
#[cfg(test)]
mod benchmark;
mod segmentation;
use segmentation::{append_utterance, Decision, Segmentation};

const LIVE_INTERVAL: Duration = Duration::from_millis(150);

pub(crate) struct LiveTask {
    stop: watch::Sender<bool>,
    task: tauri::async_runtime::JoinHandle<LiveDraft>,
}

impl LiveTask {
    pub(crate) async fn finish(self) -> Result<LiveDraft, String> {
        self.stop.send_replace(true);
        self.task
            .await
            .map_err(|_| "live dictation task failed".to_string())
    }
}

/// Owns the provisional range until finalization or cancellation, including
/// every early-return path in the stop command.
pub(crate) struct LiveDraft {
    pub(crate) injector: SystemTextInjector,
    pub(crate) monitor: Arc<InputMonitor>,
    target: TargetWindow,
    session: Option<ProvisionalInsertion>,
    checkpoint: Option<u64>,
    attempted: bool,
    defer_insertion: bool,
    pub(crate) pasted: bool,
}

impl LiveDraft {
    fn can_insert_final(&self, text: &str) -> bool {
        !text.is_empty()
            && self
                .checkpoint
                .is_some_and(|checkpoint| self.monitor.unchanged_since(checkpoint))
    }

    pub(crate) fn new(
        target: TargetWindow,
        settings: &Settings,
        active_hotkey: &str,
        mode_shortcuts: &[String],
        from_shortcut: bool,
        defer_insertion: bool,
    ) -> Self {
        let monitor = Arc::new(InputMonitor::default());
        let checkpoint = monitor
            .start_for_recording(active_hotkey, mode_shortcuts, from_shortcut)
            .then(|| monitor.checkpoint())
            .flatten();
        Self {
            injector: SystemTextInjector::new(InjectionOptions {
                restore_clipboard: settings.clipboard_restore,
            }),
            monitor,
            target,
            session: None,
            checkpoint,
            attempted: false,
            defer_insertion,
            pasted: false,
        }
    }

    pub(crate) fn update(&mut self, text: &str) -> Result<(), injection::InjectionError> {
        if text.is_empty() || self.monitor.shortcut_pending() || self.defer_insertion {
            return Ok(());
        }
        if let Some(session) = self.session.as_mut() {
            let result = self
                .injector
                .update_provisional(session, text, &self.monitor)?;
            if result != InsertResult::ClipboardPaste {
                return Err(injection::InjectionError::BackendFailure(
                    "live insertion could not be confirmed",
                ));
            }
        } else if !self.attempted {
            self.attempted = true;
            // An interaction before the first result must also block insertion.
            if self
                .checkpoint
                .is_some_and(|c| self.monitor.unchanged_since(c))
            {
                self.session = self.injector.begin_live_provisional(
                    text,
                    &self.target,
                    &self.monitor,
                    self.checkpoint.expect("checkpoint was verified"),
                )?;
            }
            self.pasted |= self.session.as_ref().is_some_and(|s| s.paste_was_queued());
            if !self.pasted {
                return Err(injection::InjectionError::BackendFailure(
                    "live insertion could not be verified",
                ));
            }
        }
        self.pasted |= self.session.as_ref().is_some_and(|s| s.paste_was_queued());
        Ok(())
    }

    pub(crate) fn finish(&mut self, text: &str) -> Result<InsertResult, injection::InjectionError> {
        if self.defer_insertion && self.session.is_none() && !self.attempted {
            self.attempted = true;
            if self.can_insert_final(text) {
                self.session = self.injector.begin_live_provisional(
                    text,
                    &self.target,
                    &self.monitor,
                    self.checkpoint.expect("checkpoint was verified"),
                )?;
                self.pasted |= self
                    .session
                    .as_ref()
                    .is_some_and(|session| session.paste_was_queued());
            }
        }
        if let Some(session) = self.session.as_mut() {
            self.injector
                .finish_provisional(session, text, &self.monitor)
        } else {
            self.injector
                .copy_to_clipboard(text)
                .map(|()| InsertResult::ClipboardOnly)
        }
    }
}

impl Drop for LiveDraft {
    fn drop(&mut self) {
        // No updater retains this session. Cancellation cleanup may now remove
        // its exact unedited range using the ordinary interaction guards.
        self.monitor.observe_cancellation(None);
        if let Some(session) = self.session.as_mut() {
            self.injector.cancel_provisional(session, &self.monitor);
        }
        self.monitor.shutdown();
    }
}

pub(crate) fn start(
    app: AppHandle,
    operation_id: u64,
    mut draft: LiveDraft,
    prompt: Option<String>,
    speech_locale: Option<String>,
    cancel: watch::Receiver<bool>,
) -> LiveTask {
    draft.monitor.observe_cancellation(Some(cancel.clone()));
    let (stop, stopped) = watch::channel(false);
    let task = tauri::async_runtime::spawn(async move {
        let services = app.state::<Services>();
        let result = run(
            &services.audio,
            services.transcriber.as_ref(),
            prompt.as_deref(),
            speech_locale.as_deref(),
            cancel,
            stopped,
            LIVE_INTERVAL,
            |text| {
                if services.lifecycle.phase() != PipelinePhase::Recording
                    || services.lifecycle.is_cancelled(operation_id)
                {
                    return;
                }
                let state = app.state::<AppState>();
                let _ = app.emit("app-state", state.publish_result(text.to_owned()));
                if draft.update(text).is_err() {
                    emit_status(&app, "live_insertion_unavailable", "Live text insertion paused. The final result will remain available in this app.");
                }
            },
        )
        .await;
        if result.is_err() && !services.lifecycle.is_cancelled(operation_id) {
            emit_status(
                &app,
                "live_transcription_failed",
                "Live transcription failed. The full recording will be transcribed after stopping.",
            );
        }
        draft
    });
    LiveTask { stop, task }
}

/// Serial snapshot inference provides backpressure: slow models never build a
/// queue. Stop drains the current request; cancellation uses the worker's
/// existing cancellation protocol. A late result is never published.
async fn run(
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    transcriber: &dyn Transcriber,
    prompt: Option<&str>,
    speech_locale: Option<&str>,
    mut cancel: watch::Receiver<bool>,
    mut stopped: watch::Receiver<bool>,
    interval: Duration,
    mut on_update: impl FnMut(&str),
) -> Result<(), String> {
    let mut segmentation = Segmentation::default();
    let mut cursor = Duration::ZERO;
    let mut completed = String::new();
    let mut displayed = String::new();
    loop {
        if *cancel.borrow() || *stopped.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = cancel.changed() => return Ok(()),
            _ = stopped.changed() => return Ok(()),
            _ = tokio::time::sleep(interval) => {},
        }
        let snapshot = {
            let audio = audio.lock().await;
            if *cancel.borrow() || *stopped.borrow() {
                return Ok(());
            }
            match audio.snapshot(cursor) {
                Ok(snapshot) => snapshot,
                Err(AudioError::NotCapturing) => return Ok(()),
                Err(error) => return Err(command_error(error)),
            }
        };
        let mut prepared = match tokio::task::spawn_blocking(move || snapshot.prepare())
            .await
            .map_err(|_| "live audio preparation failed")?
        {
            Ok(prepared) => prepared,
            Err(AudioError::TooShort { .. } | AudioError::NoVoiceDetected) => continue,
            Err(error) => return Err(command_error(error)),
        };
        let (samples, endpoint, voiced_until) = match segmentation.plan(&prepared) {
            Decision::Skip(silence) => {
                cursor += silence;
                continue;
            }
            Decision::Wait => continue,
            Decision::Decode {
                samples,
                endpoint,
                voiced_until,
            } => (samples, endpoint, voiced_until),
        };
        prepared.samples.truncate(samples);
        let decoded_duration = prepared.duration();
        let artifact = tokio::task::spawn_blocking(move || prepared.into_artifact())
            .await
            .map_err(|_| "live audio writing failed")?
            .map_err(command_error)?;
        let mut cleanup = TempArtifact::new(artifact.path.clone(), false);
        if *cancel.borrow() || *stopped.borrow() {
            return Ok(());
        }
        let inference_started = Instant::now();
        let result = transcriber
            .transcribe_with_locale(&artifact.path, prompt, speech_locale, cancel.clone())
            .await;
        let inference_cost = inference_started.elapsed();
        cleanup.cleanup().map_err(command_error)?;
        if *cancel.borrow() || *stopped.borrow() {
            return Ok(());
        }
        let transcript = result.map_err(command_error)?;
        if transcript.text.trim().is_empty() {
            return Err("live recognition returned no text for voiced audio".into());
        }
        let text = append_utterance(&completed, &transcript.text);
        if text != displayed {
            on_update(&text);
            displayed = text.clone();
        }
        segmentation.recognized(decoded_duration, voiced_until, inference_cost, endpoint);
        if endpoint {
            completed = text;
            cursor += decoded_duration;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio::{AudioArtifact, AudioFuture, AudioSnapshot, CaptureState, LevelMeter};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    fn deferred_test_draft() -> LiveDraft {
        let monitor = Arc::new(InputMonitor::default());
        monitor.test_set_available(true);
        LiveDraft {
            injector: SystemTextInjector::new(InjectionOptions::default()),
            monitor,
            target: TargetWindow {
                window_handle: 0,
                control_handle: 0,
                process_id: 0,
                thread_id: 0,
                is_secure: false,
            },
            session: None,
            checkpoint: Some(0),
            attempted: false,
            defer_insertion: true,
            pasted: false,
        }
    }

    #[test]
    fn translate_draft_defers_raw_text_and_keeps_original_checkpoint() {
        let mut draft = deferred_test_draft();
        draft.update("raw speech").unwrap();
        assert!(!draft.attempted);
        assert!(draft.session.is_none());
        assert!(!draft.pasted);
        assert!(draft.can_insert_final("translated speech"));
        draft.monitor.test_record_input();
        assert!(!draft.can_insert_final("translated speech"));
    }

    #[test]
    fn translate_final_requires_available_monitor_and_uncancelled_checkpoint() {
        let draft = deferred_test_draft();
        assert!(!draft.can_insert_final(""));
        draft.monitor.test_set_available(false);
        assert!(!draft.can_insert_final("translated speech"));
        draft.monitor.test_set_available(true);
        let (cancel, cancelled) = watch::channel(false);
        draft.monitor.observe_cancellation(Some(cancelled));
        cancel.send_replace(true);
        assert!(!draft.can_insert_final("translated speech"));
    }

    struct Capture {
        directory: PathBuf,
        snapshots: AtomicUsize,
        source: Option<Vec<f32>>,
    }
    impl AudioCapture for Capture {
        fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
            let index = self.snapshots.fetch_add(1, Ordering::SeqCst);
            let samples = self.source.clone().unwrap_or_else(|| {
                let mut samples = vec![0.0; 24_000];
                samples.extend((0..24_000 * index).map(|i| (i as f32 * 0.1).sin() * 0.2));
                samples
            });
            let offset = (since.as_secs_f64() * 24_000.0).round() as usize;
            let samples = samples[offset.min(samples.len())..].to_vec();
            Ok(AudioSnapshot::for_test(
                samples,
                CaptureConfig {
                    artifact_directory: Some(self.directory.clone()),
                    ..CaptureConfig::default()
                },
            ))
        }
        fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
            Box::pin(async { Ok(vec![]) })
        }
        fn arm(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn disarm(&mut self) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn start(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
            panic!("live inference must not stop capture")
        }
        fn cancel(&mut self) -> AudioFuture<'_, ()> {
            panic!("live inference must not cancel capture")
        }
        fn state(&self) -> CaptureState {
            CaptureState::Capturing
        }
        fn level(&self) -> LevelMeter {
            LevelMeter::default()
        }
    }

    struct Recognizer {
        calls: AtomicUsize,
        entered: Notify,
        release: Option<Arc<Notify>>,
    }
    #[async_trait::async_trait]
    impl Transcriber for Recognizer {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn transcribe(
            &self,
            path: &Path,
            _: Option<&str>,
            _: watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            assert!(path.is_file());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(asr::Transcript {
                text: if index == 0 {
                    "途中"
                } else {
                    "途中の文章"
                }
                .into(),
                segments: vec![],
                model: "test".into(),
                duration_ms: 1,
            })
        }
        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }
        async fn reconfigure(&self, _: WorkerCommand) {}
    }

    #[tokio::test]
    async fn live_recognition_skips_silence_publishes_before_stop_and_cleans_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(Capture {
                directory: directory.path().to_owned(),
                snapshots: AtomicUsize::new(0),
                source: None,
            }));
        let recognizer = Recognizer {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: None,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                cancel,
                stopped,
                Duration::from_millis(1),
                |text| {
                    assert!(!*stop.borrow());
                    updates.push(text.to_owned());
                    if updates.len() == 2 {
                        stop.send_replace(true);
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updates, ["途中", "途中の文章"]);
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(audio.lock().await.state(), CaptureState::Capturing);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn stop_or_cancel_during_inference_discards_late_text_and_drains_work() {
        for cancelling in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
                tokio::sync::Mutex::new(Box::new(Capture {
                    directory: directory.path().to_owned(),
                    snapshots: AtomicUsize::new(1),
                    source: None,
                }));
            let release = Arc::new(Notify::new());
            let recognizer = Recognizer {
                calls: AtomicUsize::new(0),
                entered: Notify::new(),
                release: Some(Arc::clone(&release)),
            };
            let (cancel_tx, cancel) = watch::channel(false);
            let (stop, stopped) = watch::channel(false);
            let operation = run(
                &audio,
                &recognizer,
                None,
                None,
                cancel,
                stopped,
                Duration::from_millis(1),
                |_| panic!("late result reached target"),
            );
            let control = async {
                recognizer.entered.notified().await;
                if cancelling {
                    cancel_tx.send_replace(true);
                } else {
                    stop.send_replace(true);
                }
                // Capture remains independently accessible while inference is running.
                assert_eq!(audio.lock().await.state(), CaptureState::Capturing);
                release.notify_one();
            };
            let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(operation, control)
            })
            .await
            .unwrap();
            result.unwrap();
            assert_eq!(recognizer.calls.load(Ordering::SeqCst), 1);
            assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn utterance_endpoint_advances_audio_without_deduplicating_repeated_words() {
        let directory = tempfile::tempdir().unwrap();
        let mut utterance: Vec<f32> = (0..28_800).map(|i| (i as f32 * 0.1).sin() * 0.2).collect();
        utterance.extend(vec![0.0; 19_200]);
        let source = [utterance.clone(), utterance].concat();
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(Capture {
                directory: directory.path().to_owned(),
                snapshots: AtomicUsize::new(0),
                source: Some(source),
            }));
        let recognizer = Recognizer {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            release: None,
        };
        let (_cancel, cancel) = watch::channel(false);
        let (stop, stopped) = watch::channel(false);
        let mut updates = vec![];
        tokio::time::timeout(
            Duration::from_secs(2),
            run(
                &audio,
                &recognizer,
                None,
                None,
                cancel,
                stopped,
                Duration::from_millis(1),
                |text| {
                    updates.push(text.to_owned());
                    if updates.len() == 2 {
                        stop.send_replace(true);
                    }
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updates, ["途中", "途中途中の文章"]);
        assert_eq!(recognizer.calls.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

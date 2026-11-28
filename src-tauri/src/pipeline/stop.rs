//! Shared stop/capture/recognition lifecycle and mode dispatch.
use super::{
    finish::{report_history_save_warning, save_history_reporting_row, HISTORY_CHANGED_EVENT},
    PipelineContext, RecordedTake, StopTimings,
};
use crate::commands::{
    cancellation_copy, ensure_model_loaded_cancellable, failed_take_history_item,
    failed_take_message, microphone_disconnected, play_stop_cue, preserve_failed_take,
    processing_message, report_temp_cleanup, take_published_sessions, StopReason,
    TranscriptionFailure, INSERTION_FAILED, MICROPHONE_DISCONNECTED,
    MICROPHONE_DISCONNECTED_WITHOUT_SPEECH, RECORDING_LIMIT_REACHED,
};
use crate::{
    app_context, asr, command_error, emit_stage, emit_state, emit_status, model_identity,
    recording_overlay,
    state::{AppState, PipelineMode},
    storage::Storage,
    types::{AppPhase, NewHistoryItem, RecordingResult},
    Services, TempArtifact,
};
use std::{sync::OnceLock, time::Instant};
use tauri::{AppHandle, Emitter};

use super::orchestration::{RecordingDriver, RecordingEvents};
use super::speculative::{CorrectionInputs, Speculation};
use super::take_trace::{TakeTrace, PENDING};
use crate::live_dictation::LiveDraft;
use crate::storage::{TakeOutcome, TakeSpan, TakeStage};
use crate::{
    audio::AudioArtifact,
    state::PipelineLifecycle,
    types::{AppStage, Settings},
};
use crate::{smart_dictate::SmartRoute, AskSession, EditSession};
use tokio::sync::watch;

const NO_VOICE_CAPTURED: &str = "No speech was detected. Your audio may be too quiet. Check your microphone and Input gain in Input settings.";
const INTERRUPTED_CAPTURE_REJECTED: &str =
    "Microphone input stopped before enough speech was captured for automatic transcription.";

/// Energy detection cannot distinguish silence from speech below its floor.
/// Use the same actionable copy for the overlay and the IPC caller so a
/// hotkey/UI error does not replace it with an untranslated diagnostic.
pub(super) fn report_capture_failure(
    error: crate::audio::AudioError,
    disconnected: bool,
    publish: impl FnOnce(&'static str),
) -> String {
    let no_voice = matches!(error, crate::audio::AudioError::NoVoiceDetected) && !disconnected;
    let message = capture_failure_message(no_voice, disconnected);
    publish(message);
    if no_voice {
        message.to_owned()
    } else {
        command_error(error)
    }
}

fn capture_failure_message(no_voice: bool, disconnected: bool) -> &'static str {
    if disconnected {
        MICROPHONE_DISCONNECTED_WITHOUT_SPEECH
    } else if no_voice {
        NO_VOICE_CAPTURED
    } else {
        "No usable speech was captured. Start a new recording and try again."
    }
}

struct Desktop<'a> {
    app: &'a AppHandle,
    services: &'a Services,
    state: &'a AppState,
    storage: &'a Storage,
    operation_id: OnceLock<u64>,
}

struct Captured {
    checkpoint: Option<crate::audio::spool::Checkpoint>,
    settings: Settings,
    artifact: AudioArtifact,
    artifact_cleanup: TempArtifact,
    started: Instant,
    disconnected: bool,
    stop_timings: StopTimings,
    translation_target: Option<String>,
    draft: Option<LiveDraft>,
    live_session_lost: bool,
    edit_session: Option<EditSession>,
    ask_session: Option<AskSession>,
    smart_route: Option<SmartRoute>,
}

impl RecordingEvents for Desktop<'_> {
    fn state(&self) -> &AppState {
        self.state
    }
    fn stage(&self, stage: AppStage, message: &str) {
        emit_stage(self.app, self.state, stage, message);
    }
    fn interactive(&self, interactive: bool) {
        recording_overlay::set_interactive(self.app, interactive);
    }
}

impl RecordingDriver for Desktop<'_> {
    type Captured = Captured;
    type Recognized = (
        RecordedTake,
        Option<EditSession>,
        Option<AskSession>,
        bool,
        Option<crate::audio::spool::Checkpoint>,
    );
    fn lifecycle(&self) -> &PipelineLifecycle {
        &self.services.lifecycle
    }
    fn begin_trace(&self, id: u64, mode: PipelineMode) -> TakeTrace {
        let _ = self.operation_id.set(id);
        // Dictation tests record nothing, like their metrics.
        if self.services.lifecycle.is_dictation_test(id) {
            return TakeTrace::disabled();
        }
        PENDING
            .claim(id)
            .unwrap_or_else(|| TakeTrace::new(self.storage, mode))
    }
    async fn capture(
        &self,
        mode: PipelineMode,
        reason: StopReason,
        started: Instant,
        trace: &TakeTrace,
    ) -> Result<Captured, String> {
        let Self {
            app,
            services,
            state,
            storage,
            ..
        } = *self;
        let (live_task, edit_session, ask_session, translation_target) = take_published_sessions(
            &services.live,
            &services.edit,
            &services.ask,
            &services.voice_translation_target,
        )
        .await?;
        // A Smart Dictate/Edit take keeps the Dictate lifecycle of its key
        // but is processed in the mode it was routed to at the press.
        let smart_route = services
            .smart_route
            .lock()
            .map_err(|_| "target service is unavailable".to_string())?
            .take();
        let mode = smart_route.map_or(mode, SmartRoute::mode);
        // Stop live recognition first so it starts no window decode while the
        // capture is finalized. It is joined below, after the stop cue.
        if let Some(task) = &live_task {
            task.signal_stop();
        }
        let audio_stop_started = Instant::now();
        let mut audio = services.audio.lock().await;
        let checkpoint = audio.checkpoint();
        if let Some(checkpoint) = &checkpoint {
            let id = *self.operation_id.get().expect("stop claimed an operation");
            storage.mark_stopping_checkpoint(id, checkpoint.clone());
            if services.lifecycle.is_cancelled(id) {
                audio.cancel().await.map_err(command_error)?;
                return Err(cancellation_copy(mode).1.into());
            }
        }
        let artifact_result = audio.stop().await;
        let audio_stop_ms = audio_stop_started.elapsed().as_millis() as i64;
        trace.span(TakeSpan::AudioStop, audio_stop_ms);
        // `stop` closes the input stream. Do not re-arm it while transcription is
        // running: on Bluetooth headsets an open microphone selects the low-quality
        // HFP playback profile until the stream is released.
        drop(audio);
        let stop_settings = storage.get_settings();
        play_stop_cue(
            stop_settings
                .as_ref()
                .map(|settings| settings.interaction_sounds)
                .unwrap_or(false),
        );
        // Warm the AI provider connection while the live task drains and the
        // recording is recognized.
        if let Ok(settings) = &stop_settings {
            if artifact_result
                .as_ref()
                .is_ok_and(|artifact| !artifact.retry_only)
                && services.lifecycle.dictation_test_id().is_none()
            {
                crate::correction::prewarm_connection(settings, mode);
            }
        }
        // Keep the full artifact owned even if joining live work or loading settings fails.
        let mut artifact_cleanup = artifact_result
            .as_ref()
            .ok()
            .map(|artifact| TempArtifact::new(artifact.path.clone(), false));
        // Stop waits for an in-flight live window before the final recognition.
        let live_drain_started = Instant::now();
        let live_drained = live_task.is_some();
        let (draft, live_drain_failed) = match live_task {
            Some(task) => match task.finish().await {
                Ok(draft) => (Some(draft), false),
                Err(_) => (None, true),
            },
            None => (None, false),
        };
        // Edit and Ask use a recognition-only live task. Their target
        // ownership is carried by the separate edit and ask sessions.
        let live_session_lost = draft.is_none()
            && matches!(mode, PipelineMode::Dictate | PipelineMode::Translate)
            && services.lifecycle.dictation_test_id().is_none();
        let live_drain_ms = live_drained.then(|| live_drain_started.elapsed().as_millis() as i64);
        if let Some(live_drain_ms) = live_drain_ms {
            trace.span(TakeSpan::LiveDrain, live_drain_ms);
        }
        let settings = storage.get_settings().map_err(command_error)?;
        let disconnected = microphone_disconnected(reason, &artifact_result);
        let artifact = artifact_result.map_err(|error| {
            report_capture_failure(error, disconnected, |message| {
                emit_state(app, state, AppPhase::Error, message);
            })
        })?;
        if disconnected && !artifact.retry_only {
            emit_status(app, "microphone_disconnected", MICROPHONE_DISCONNECTED);
        } else if !disconnected && matches!(reason, StopReason::MaximumDuration(_)) {
            emit_status(app, "recording_limit_reached", RECORDING_LIMIT_REACHED);
        }
        let mut artifact_cleanup = artifact_cleanup
            .take()
            .expect("successful audio has cleanup ownership");
        // The temporary capture remains cleanup-owned until any retained History
        // copy has committed. History never takes ownership of this path.
        artifact_cleanup.retain = false;

        Ok(Captured {
            checkpoint,
            settings,
            artifact,
            artifact_cleanup,
            started,
            disconnected,
            stop_timings: StopTimings {
                audio_stop_ms,
                live_drain_ms: live_drain_ms.filter(|_| !live_drain_failed),
                live_final_match: None,
            },
            translation_target,
            draft,
            live_session_lost,
            edit_session,
            ask_session,
            smart_route,
        })
    }
    fn transcription_message(&self, capture: &Captured, reason: StopReason) -> &'static str {
        if capture.artifact.retry_only {
            return INTERRUPTED_CAPTURE_REJECTED;
        }
        processing_message(
            reason,
            capture.disconnected,
            if capture.settings.asr_backend == "openai-compatible" {
                "Transcribing with the configured speech provider."
            } else {
                "Transcribing locally."
            },
        )
    }
    async fn recognize(
        &self,
        operation_id: u64,
        mode: PipelineMode,
        capture: Captured,
        cancel: watch::Receiver<bool>,
        trace: &TakeTrace,
    ) -> Result<Self::Recognized, String> {
        if capture.artifact.retry_only {
            return Err(self.recover_interrupted_capture(operation_id, mode, capture, trace));
        }
        let Self {
            app,
            services,
            state,
            storage,
            ..
        } = *self;
        let Captured {
            checkpoint,
            settings,
            artifact,
            mut artifact_cleanup,
            started,
            stop_timings,
            translation_target,
            mut draft,
            live_session_lost,
            edit_session,
            ask_session,
            smart_route,
            ..
        } = capture;
        let mode = smart_route.map_or(mode, SmartRoute::mode);
        let duration_ms = artifact.duration.as_millis() as u64;
        let (cancel_message, cancel_error) = cancellation_copy(mode);
        trace.stage(TakeStage::Model);
        let app_context = services
            .app_context
            .lock()
            .map_err(|_| "target service is unavailable".to_string())?
            .clone();
        let prompt = storage
            .dictionary_asr_prompt_for(
                crate::app_context::asr_prompt_context(mode, app_context.as_ref()),
                &settings.asr_backend,
            )
            .unwrap_or_default();
        let metrics = storage.metrics_for(
            mode,
            crate::app_context::history_category(app_context.as_ref()),
        );
        if live_session_lost && !services.lifecycle.is_dictation_test(operation_id) {
            let _ = metrics.add_metric(
                "live_drain",
                Some(&settings.asr_backend),
                None,
                false,
                Some("live_session_lost"),
            );
        }
        // Stop-path timings are written once the take ends here or, after
        // delivery, by `deliver`. Dictation tests record none.
        let record_stop_timings = || {
            if !services.lifecycle.is_dictation_test(operation_id) {
                stop_timings.record(metrics, &settings.asr_backend);
            }
        };
        let correction_cancel = cancel.clone();
        // Opt-in: start correcting the last live hypothesis now, after stop,
        // in parallel with final recognition. Dropping it cancels it.
        let speculation = if !settings.speculative_correction
            || services.lifecycle.is_dictation_test(operation_id)
            || services.lifecycle.is_cancelled(operation_id)
        {
            None
        } else {
            Speculation::begin(
                &settings,
                mode,
                draft.as_ref().and_then(LiveDraft::last_hypothesis),
                |hypothesis| {
                    CorrectionInputs::resolve(storage, &settings, hypothesis, app_context.as_ref())
                },
                cancel.clone(),
                super::speculative::provider_correction,
            )
        };
        // Defense in depth: model changes are refused while a recording is
        // active, but if the model is not marked ready (for example its load
        // failed, or a worker crash lost it), load it as a new recording would.
        let model_ensure_started = Instant::now();
        let model_ready =
            ensure_model_loaded_cancellable(app, services, &settings, Some(cancel.clone())).await;
        trace.span_since(TakeSpan::ModelEnsure, model_ensure_started);
        if model_ready.is_ok() {
            trace.stage(TakeStage::Asr);
        }
        if services.lifecycle.is_cancelled(operation_id) {
            record_stop_timings();
            report_temp_cleanup(app, &mut artifact_cleanup);
            emit_state(app, state, AppPhase::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        let asr_started = Instant::now();
        let model_was_ready = model_ready.is_ok();
        let stream_transcript = if model_was_ready {
            if let Some(stream) = draft.as_mut().and_then(|draft| draft.streaming.take()) {
                let result = stream
                    .finish(services.transcriber.as_ref(), &settings, cancel.clone())
                    .await;
                if !services.lifecycle.is_dictation_test(operation_id)
                    && !matches!(result, Err(asr::AsrError::Cancelled))
                {
                    let _ = metrics.add_metric(
                        "asr_streaming",
                        Some(&settings.asr_backend),
                        Some(asr_started.elapsed().as_millis() as i64),
                        result.is_ok(),
                        result.is_err().then_some("stream_fallback"),
                    );
                }
                match result {
                    Ok(value) => Some(Ok(value)),
                    Err(asr::AsrError::Cancelled) => Some(Err(asr::AsrError::Cancelled)),
                    Err(_) => None,
                }
            } else {
                None
            }
        } else {
            None
        };
        let transcript_result = match model_ready {
            Err(error) => Err((error, TranscriptionFailure::Retryable)),
            Ok(_) => match if let Some(result) = stream_transcript {
                result
            } else {
                crate::live_dictation::incremental::transcribe_final(
                    services.transcriber.as_ref(),
                    draft.as_mut().and_then(|draft| draft.incremental.take()),
                    &settings,
                    &artifact.path,
                    prompt.as_deref(),
                    cancel,
                    |outcome| {
                        if !services.lifecycle.is_dictation_test(operation_id) {
                            outcome.record(metrics, &settings.asr_backend);
                        }
                    },
                )
                .await
            } {
                Ok(value) => {
                    trace.span_since(TakeSpan::Asr, asr_started);
                    trace.final_ready();
                    Ok(value)
                }
                Err(asr::AsrError::Cancelled) => {
                    trace.span_since(TakeSpan::Asr, asr_started);
                    record_stop_timings();
                    report_temp_cleanup(app, &mut artifact_cleanup);
                    emit_state(app, state, AppPhase::Idle, cancel_message);
                    return Err(cancel_error.into());
                }
                Err(error) => {
                    trace.span_since(TakeSpan::Asr, asr_started);
                    let failure = TranscriptionFailure::of(&error);
                    let message = if failure == TranscriptionFailure::CpuMemory {
                        crate::commands::CPU_MEMORY_FAILURE.to_owned()
                    } else {
                        command_error(error)
                    };
                    Err((message, failure))
                }
            },
        };
        let transcript = match transcript_result {
            Ok(value) => value,
            Err((error, failure)) => {
                if services.lifecycle.is_cancelled(operation_id) {
                    record_stop_timings();
                    report_temp_cleanup(app, &mut artifact_cleanup);
                    emit_state(app, state, AppPhase::Idle, cancel_message);
                    return Err(cancel_error.into());
                }
                // Failed recognition can keep a History row. Claim its terminal
                // disposition before a concurrent cancel could accept the take.
                services
                    .lifecycle
                    .commit_side_effect(operation_id, || Ok::<_, std::convert::Infallible>(()))
                    .map_err(str::to_owned)?
                    .expect("infallible cancellation fence");
                let dictation_test = services.lifecycle.is_dictation_test(operation_id);
                let _ = if dictation_test {
                    Ok(())
                } else {
                    metrics.add_metric(
                        "asr",
                        Some(settings.asr_backend.as_str()),
                        model_was_ready.then(|| asr_started.elapsed().as_millis() as i64),
                        false,
                        Some("asr_failed"),
                    )
                };
                record_stop_timings();
                let asr_provider = model_identity(&settings)
                    .0
                    .unwrap_or_else(|| settings.asr_backend.clone());
                let preserved = if dictation_test {
                    None
                } else {
                    preserve_failed_take(
                        &storage.for_recorded_history(checkpoint.clone()),
                        &settings,
                        failed_take_history_item(
                            mode,
                            &asr_provider,
                            edit_session
                                .as_ref()
                                .map(|session| session.selection.text()),
                            translation_target.as_deref(),
                            app_context::history_category(app_context.as_ref()),
                            duration_ms,
                        )
                        .map(|item| NewHistoryItem {
                            insertion_detail: failure.history_detail(),
                            ..item
                        }),
                        &artifact.path,
                    )
                };
                emit_state(
                    app,
                    state,
                    AppPhase::Error,
                    failed_take_message(preserved, failure),
                );
                // History keeps its own copy, so the temporary capture is always removed.
                report_temp_cleanup(app, &mut artifact_cleanup);
                return Err(error);
            }
        };
        if services.lifecycle.is_cancelled(operation_id) {
            record_stop_timings();
            emit_state(app, state, AppPhase::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        let _ = if services.lifecycle.is_dictation_test(operation_id) {
            Ok(())
        } else {
            metrics.add_metric(
                "asr",
                Some(&settings.asr_backend),
                Some(asr_started.elapsed().as_millis() as i64),
                true,
                None,
            )
        };
        services
            .target
            .lock()
            .map_err(|_| "target service is unavailable".to_string())?
            .take();

        let stop_timings = StopTimings {
            live_final_match: super::live_final_match(
                mode,
                draft.as_ref().and_then(LiveDraft::last_hypothesis),
                &transcript.text,
            ),
            ..stop_timings
        };
        let take = RecordedTake {
            automatic: false,
            settings,
            transcript,
            artifact,
            artifact_cleanup,
            started,
            correction_cancel,
            mode,
            app_context,
            translation_target,
            draft,
            speculation,
            stop_timings,
            trace: trace.clone(),
            smart_route,
        };
        Ok((
            take,
            edit_session,
            ask_session,
            live_session_lost,
            checkpoint,
        ))
    }
    async fn deliver(
        &self,
        operation_id: u64,
        // The recognized take carries the mode it is processed in.
        _mode: PipelineMode,
        recognized: Self::Recognized,
        trace: &TakeTrace,
    ) -> Result<RecordingResult, String> {
        let Self {
            app,
            services,
            state,
            storage,
            ..
        } = *self;
        let (mut take, mut edit_session, mut ask_session, live_session_lost, checkpoint) =
            recognized;
        let history_storage = storage.for_recorded_history(checkpoint);
        let context = PipelineContext {
            app,
            services,
            state,
            storage: &history_storage,
            operation_id,
        };
        if services.lifecycle.is_dictation_test(operation_id) {
            return super::setup_tests::finish_dictation_test(context, take).await;
        }
        // Attributed to the recognized mode, like the `asr` metric of the
        // same take, even when automatic routing changes the delivered mode.
        let metric_mode = take.mode;
        // Captured before the mode finish consumes the take; written after it.
        let stop_timings = take.stop_timings;
        let metric_context = take.app_context.clone();
        let asr_backend = take.settings.asr_backend.clone();
        let record_stop_timings = || {
            stop_timings.record(
                storage.metrics_for(
                    metric_mode,
                    app_context::history_category(metric_context.as_ref()),
                ),
                &asr_backend,
            );
        };
        if live_session_lost {
            let result = recover_live_transcript(context, take);
            record_stop_timings();
            return result;
        }
        let routed =
            super::routing::route(&context, &mut take, &mut edit_session, &mut ask_session).await;
        if let Err(error) = routed {
            record_stop_timings();
            if services.lifecycle.is_cancelled(operation_id) {
                let (message, _) = cancellation_copy(take.mode);
                emit_state(app, state, AppPhase::Idle, message);
            } else if state.phase() != Some(AppPhase::Error) {
                emit_state(app, state, AppPhase::Error, "Voice mode could not be completed. The transcript remains available in this app. Check Models and Privacy, or use a dedicated mode shortcut.");
            }
            report_temp_cleanup(app, &mut take.artifact_cleanup);
            return Err(error);
        }
        // A speculative Dictate correction cannot serve another mode.
        if take.mode != PipelineMode::Dictate {
            if let Some(speculation) = take.speculation.take() {
                speculation.discard(
                    storage.metrics_for(
                        metric_mode,
                        app_context::history_category(metric_context.as_ref()),
                    ),
                    &take.settings.correction_provider,
                );
            }
        }
        trace.stage(TakeStage::Ai);
        let result = match take.mode {
            PipelineMode::Edit => super::edit::finish(context, take, edit_session).await,
            PipelineMode::Ask => super::ask::finish(context, take, ask_session).await,
            PipelineMode::Dictate | PipelineMode::Translate => {
                super::dictate_translate::finish(context, take).await
            }
            PipelineMode::SelectedText => unreachable!("selected text does not record audio"),
        };
        record_stop_timings();
        result
    }
}

impl Desktop<'_> {
    fn recover_interrupted_capture(
        &self,
        operation_id: u64,
        mode: PipelineMode,
        capture: Captured,
        trace: &TakeTrace,
    ) -> String {
        let mode = capture.smart_route.map_or(mode, SmartRoute::mode);
        trace.stage(TakeStage::Capture);
        if self.services.lifecycle.is_dictation_test(operation_id) {
            emit_state(
                self.app,
                self.state,
                AppPhase::Error,
                MICROPHONE_DISCONNECTED_WITHOUT_SPEECH,
            );
            return MICROPHONE_DISCONNECTED_WITHOUT_SPEECH.into();
        }
        let completion = self.storage.reserve_completion();
        if let Err(error) = self
            .services
            .lifecycle
            .commit_side_effect(operation_id, || Ok::<_, std::convert::Infallible>(()))
        {
            if self.services.lifecycle.is_cancelled(operation_id) {
                let (message, error) = cancellation_copy(mode);
                emit_state(self.app, self.state, AppPhase::Idle, message);
                return error.into();
            }
            return error.into();
        }
        emit_state(
            self.app,
            self.state,
            AppPhase::Error,
            INTERRUPTED_CAPTURE_REJECTED,
        );
        let app = self.app.clone();
        let storage = self
            .storage
            .for_recorded_history(capture.checkpoint.clone());
        let Captured {
            settings,
            artifact,
            mut artifact_cleanup,
            edit_session,
            translation_target,
            ..
        } = capture;
        completion.defer(move || {
            let asr_provider = model_identity(&settings)
                .0
                .unwrap_or_else(|| settings.asr_backend.clone());
            let item = failed_take_history_item(
                mode,
                &asr_provider,
                edit_session
                    .as_ref()
                    .map(|session| session.selection.text()),
                translation_target.as_deref(),
                None,
                artifact.duration.as_millis() as u64,
            );
            if let Some(retention) = preserve_failed_take(&storage, &settings, item, &artifact.path)
            {
                let _ = app.emit(HISTORY_CHANGED_EVENT, ());
                let message = match retention {
                    crate::storage::FailedTakeRetention::Normal => {
                        "The interrupted recording was saved to History. You can retry it there."
                    }
                    crate::storage::FailedTakeRetention::Temporary => {
                        "The interrupted recording is available to retry from History for 24 hours."
                    }
                };
                emit_status(&app, "interrupted_recording_saved", message);
            }
            report_temp_cleanup(&app, &mut artifact_cleanup);
        });
        INTERRUPTED_CAPTURE_REJECTED.into()
    }
}

/// The full take still has a transcript, but its preview owner and insertion
/// state were lost. Recover only the transcript through the app and History.
fn recover_live_transcript(
    context: PipelineContext<'_>,
    mut take: RecordedTake,
) -> Result<RecordingResult, String> {
    let PipelineContext {
        app,
        services,
        state,
        storage,
        operation_id,
    } = context;
    take.trace.stage(TakeStage::Delivery);
    // Reserve before publishing so a privacy deletion waits for the
    // recovered take. Keep the potentially slow WAV copy off the lifecycle lock.
    let completion = storage.reserve_completion();
    if let Err(error) = services
        .lifecycle
        .commit_side_effect(operation_id, || Ok::<_, std::convert::Infallible>(()))
    {
        if services.lifecycle.is_cancelled(operation_id) {
            let (message, error) = cancellation_copy(take.mode);
            emit_state(app, state, AppPhase::Idle, message);
            return Err(error.into());
        }
        return Err(error.into());
    }
    take.trace.outcome(TakeOutcome::InsertionFailed);
    crate::evaluation::insertion_detail(operation_id, Some("backend_failure"));
    let _ = app.emit(
        "app-state",
        state.publish_result(take.transcript.text.clone()),
    );
    emit_state(app, state, AppPhase::Error, INSERTION_FAILED);
    let app = app.clone();
    let storage = storage.clone();
    let latency_ms = take.started.elapsed().as_millis() as i64;
    completion.defer(move || {
        let (history_status, saved) = save_history_reporting_row(
            &storage,
            &NewHistoryItem {
                transcript_text: &take.transcript.text,
                processed_text: None,
                source_text: None,
                instruction_text: None,
                action_kind: None,
                search_site: None,
                mode: "faithful",
                asr_provider: &take.transcript.model,
                llm_provider: None,
                target_language: None,
                app_category: app_context::history_category(take.app_context.as_ref()),
                duration_ms: Some(take.artifact.duration.as_millis() as i64),
                latency_ms: Some(latency_ms),
                retry_of_id: None,
                insertion_result: Some("insertion_failed"),
                insertion_detail: Some("backend_failure"),
            },
            Some(&take.artifact.path),
        );
        if saved {
            let _ = app.emit(HISTORY_CHANGED_EVENT, ());
        }
        report_history_save_warning(&app, history_status);
        report_temp_cleanup(&app, &mut take.artifact_cleanup);
    });
    Err(INSERTION_FAILED.into())
}

pub(crate) async fn run(
    app: &AppHandle,
    services: &Services,
    state: &AppState,
    storage: &Storage,
    reason: StopReason,
) -> Result<RecordingResult, String> {
    let driver = Desktop {
        app,
        services,
        state,
        storage,
        operation_id: OnceLock::new(),
    };
    let result = super::orchestration::run(&driver, reason).await;
    if let Some(&id) = driver.operation_id.get() {
        storage.clear_stopping_checkpoint(id);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::AudioError;

    #[test]
    fn rejected_speech_has_localized_recovery_guidance() {
        let state = AppState::default();
        let error = report_capture_failure(AudioError::NoVoiceDetected, false, |message| {
            state.transition_stage(AppStage::Error, Some(message.into()));
        });
        let snapshot = state.snapshot();
        assert_eq!(
            snapshot.message_code.as_deref(),
            Some("capture_no_voice_detected"),
        );
        assert_eq!(snapshot.message.as_deref(), Some(error.as_str()));
        assert!(error.contains("may be too quiet"));
        assert!(error.contains("Input gain"));
    }

    #[test]
    fn microphone_failure_takes_precedence_over_the_speech_gate() {
        let error = report_capture_failure(AudioError::NoVoiceDetected, true, |message| {
            assert_eq!(message, MICROPHONE_DISCONNECTED_WITHOUT_SPEECH);
        });
        assert_eq!(error, command_error(AudioError::NoVoiceDetected));
        let error = report_capture_failure(
            AudioError::StreamFailure("disconnected".into()),
            true,
            |message| {
                assert_eq!(message, MICROPHONE_DISCONNECTED_WITHOUT_SPEECH);
            },
        );
        assert_eq!(
            error,
            command_error(AudioError::StreamFailure("disconnected".into()))
        );
    }
}

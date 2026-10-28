//! Shared stop/capture/recognition lifecycle and mode dispatch.
use super::{PipelineContext, RecordedTake};
use crate::commands::{
    cancellation_copy, ensure_model_loaded_cancellable, failed_take_history_item,
    failed_take_message, microphone_disconnected, play_stop_cue, preserve_failed_take,
    processing_message, report_temp_cleanup, take_published_sessions, StopReason,
    TranscriptionFailure, MICROPHONE_DISCONNECTED, MICROPHONE_DISCONNECTED_WITHOUT_SPEECH,
    RECORDING_LIMIT_REACHED,
};
use crate::{
    app_context, asr, command_error, emit_stage, emit_state, emit_status, model_identity,
    recording_overlay,
    state::{AppState, PipelineMode},
    storage::Storage,
    types::{AppPhase, NewHistoryItem, RecordingResult},
    Services, TempArtifact,
};
use std::time::Instant;
use tauri::AppHandle;

use super::orchestration::{RecordingDriver, RecordingEvents};
use crate::live_dictation::LiveDraft;
use crate::{
    audio::AudioArtifact,
    state::PipelineLifecycle,
    types::{AppStage, Settings},
};
use crate::{AskSession, EditSession};
use tokio::sync::watch;

#[derive(Clone, Copy)]
struct Desktop<'a> {
    app: &'a AppHandle,
    services: &'a Services,
    state: &'a AppState,
    storage: &'a Storage,
}

struct Captured {
    settings: Settings,
    artifact: AudioArtifact,
    artifact_cleanup: TempArtifact,
    started: Instant,
    disconnected: bool,
    translation_target: Option<String>,
    draft: Option<LiveDraft>,
    edit_session: Option<EditSession>,
    ask_session: Option<AskSession>,
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
    type Recognized = (RecordedTake, Option<EditSession>, Option<AskSession>);
    fn lifecycle(&self) -> &PipelineLifecycle {
        &self.services.lifecycle
    }
    async fn capture(
        &self,
        mode: PipelineMode,
        reason: StopReason,
        started: Instant,
    ) -> Result<Captured, String> {
        let Self {
            app,
            services,
            state,
            storage,
        } = *self;
        let (live_task, edit_session, ask_session, translation_target) = take_published_sessions(
            &services.live,
            &services.edit,
            &services.ask,
            &services.voice_translation_target,
        )
        .await?;
        let mut audio = services.audio.lock().await;
        let artifact_result = audio.stop().await;
        // `stop` closes the input stream. Do not re-arm it while transcription is
        // running: on Bluetooth headsets an open microphone selects the low-quality
        // HFP playback profile until the stream is released.
        drop(audio);
        play_stop_cue(
            storage
                .get_settings()
                .map(|settings| settings.interaction_sounds)
                .unwrap_or(false),
        );
        // Keep the full artifact owned even if joining live work or loading settings fails.
        let mut artifact_cleanup = artifact_result
            .as_ref()
            .ok()
            .map(|artifact| TempArtifact::new(artifact.path.clone(), false));
        let draft = match live_task {
            Some(task) => Some(task.finish().await?),
            None if mode == PipelineMode::Edit
                || mode == PipelineMode::Ask
                || services.lifecycle.dictation_test_id().is_some() =>
            {
                None
            }
            None => return Err("live dictation session is unavailable".into()),
        };
        let settings = storage.get_settings().map_err(command_error)?;
        let disconnected = microphone_disconnected(reason, &artifact_result);
        let artifact = artifact_result.map_err(|error| {
            emit_state(
                app,
                state,
                AppPhase::Error,
                if disconnected {
                    MICROPHONE_DISCONNECTED_WITHOUT_SPEECH
                } else {
                    "No usable speech was captured. Start a new recording and try again."
                },
            );
            command_error(error)
        })?;
        if disconnected {
            emit_status(app, "microphone_disconnected", MICROPHONE_DISCONNECTED);
        } else if reason == StopReason::MaximumDuration {
            emit_status(app, "recording_limit_reached", RECORDING_LIMIT_REACHED);
        }
        let mut artifact_cleanup = artifact_cleanup
            .take()
            .expect("successful audio has cleanup ownership");
        // The temporary capture remains cleanup-owned until any retained History
        // copy has committed. History never takes ownership of this path.
        artifact_cleanup.retain = false;

        Ok(Captured {
            settings,
            artifact,
            artifact_cleanup,
            started,
            disconnected,
            translation_target,
            draft,
            edit_session,
            ask_session,
        })
    }
    fn transcription_message(&self, capture: &Captured, reason: StopReason) -> &'static str {
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
    ) -> Result<Self::Recognized, String> {
        let Self {
            app,
            services,
            state,
            storage,
        } = *self;
        let Captured {
            settings,
            artifact,
            mut artifact_cleanup,
            started,
            translation_target,
            draft,
            edit_session,
            ask_session,
            ..
        } = capture;
        let duration_ms = artifact.duration.as_millis() as u64;
        let (cancel_message, cancel_error) = cancellation_copy(mode);
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
        let correction_cancel = cancel.clone();
        // Defense in depth: model changes are refused while a recording is
        // active, but if the model is not marked ready (for example its load
        // failed, or a worker crash lost it), load it as a new recording would.
        let model_ready =
            ensure_model_loaded_cancellable(app, services, &settings, Some(cancel.clone())).await;
        if services.lifecycle.is_cancelled(operation_id) {
            report_temp_cleanup(app, &mut artifact_cleanup);
            emit_state(app, state, AppPhase::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        let asr_started = Instant::now();
        let model_was_ready = model_ready.is_ok();
        let transcript_result = match model_ready {
            Err(error) => Err((error, TranscriptionFailure::Retryable)),
            Ok(_) => match services
                .transcriber
                .transcribe_with_locale(
                    &artifact.path,
                    prompt.as_deref(),
                    settings.speech_locale.as_deref(),
                    cancel,
                )
                .await
            {
                Ok(value) => Ok(value),
                Err(asr::AsrError::Cancelled) => {
                    report_temp_cleanup(app, &mut artifact_cleanup);
                    emit_state(app, state, AppPhase::Idle, cancel_message);
                    return Err(cancel_error.into());
                }
                Err(error) => {
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
                let asr_provider = model_identity(&settings)
                    .0
                    .unwrap_or_else(|| settings.asr_backend.clone());
                let preserved = if dictation_test {
                    None
                } else {
                    preserve_failed_take(
                        storage,
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
        };
        Ok((take, edit_session, ask_session))
    }
    async fn deliver(
        &self,
        operation_id: u64,
        _mode: PipelineMode,
        recognized: Self::Recognized,
    ) -> Result<RecordingResult, String> {
        let Self {
            app,
            services,
            state,
            storage,
        } = *self;
        let (mut take, mut edit_session, mut ask_session) = recognized;
        let context = PipelineContext {
            app,
            services,
            state,
            storage,
            operation_id,
        };
        if services.lifecycle.is_dictation_test(operation_id) {
            return super::setup_tests::finish_dictation_test(context, take).await;
        }
        if let Err(error) =
            super::routing::route(&context, &mut take, &mut edit_session, &mut ask_session).await
        {
            if services.lifecycle.is_cancelled(operation_id) {
                let (message, _) = cancellation_copy(take.mode);
                emit_state(app, state, AppPhase::Idle, message);
            } else if state.phase() != Some(AppPhase::Error) {
                emit_state(app, state, AppPhase::Error, "Voice mode could not be completed. The transcript remains available in this app. Check Models and Privacy, or use a dedicated mode shortcut.");
            }
            report_temp_cleanup(app, &mut take.artifact_cleanup);
            return Err(error);
        }
        match take.mode {
            PipelineMode::Edit => super::edit::finish(context, take, edit_session).await,
            PipelineMode::Ask => super::ask::finish(context, take, ask_session).await,
            PipelineMode::Dictate | PipelineMode::Translate => {
                super::dictate_translate::finish(context, take).await
            }
            PipelineMode::SelectedText => unreachable!("selected text does not record audio"),
        }
    }
}

pub(crate) async fn run(
    app: &AppHandle,
    services: &Services,
    state: &AppState,
    storage: &Storage,
    reason: StopReason,
) -> Result<RecordingResult, String> {
    super::orchestration::run(
        &Desktop {
            app,
            services,
            state,
            storage,
        },
        reason,
    )
    .await
}

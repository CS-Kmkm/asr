//! History reprocessing with cancellation and retained-audio ownership.
use super::{classify_ai_stage, AiStageResult};
use crate::commands::{
    ai_command_error, correction_failure_metric_code, correction_failure_status,
    ensure_model_loaded_cancellable, history_pipeline_mode, report_history_save_warning,
    retry_app_context, retry_dictate_history_mode, retry_voice_mode, save_retry_result,
    stored_ask_action, stored_search_site, HistorySaveStatus,
};
use crate::{
    ask, command_error,
    correction::{self, FailureOutcome},
    emit_ai_failure, emit_stage, emit_state, emit_status, recording_overlay,
    state::{AppState, PipelineMode},
    storage::Storage,
    types::{self, AppPhase, NewHistoryItem, RecordingResult},
    PipelineGuard, Services, TempArtifact,
};
use crate::{commands::begin_history_retry, personalization};
use std::time::Instant;
use tauri::{AppHandle, Emitter};

pub(crate) async fn run(
    id: i64,
    app: &AppHandle,
    services: &Services,
    state: &AppState,
    storage: &Storage,
) -> Result<RecordingResult, String> {
    let source = storage
        .history_item(id)
        .map_err(command_error)?
        .ok_or_else(|| "history item not found".to_string())?;
    let mode = history_pipeline_mode(&source.mode)?;
    let (operation_id, cancel) = begin_history_retry(services, mode)?;
    let _guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    // Retry has no target window; show its overlay where the user started it,
    // not on the last take's monitor.
    recording_overlay::anchor_to_foreground();
    let mut retry_cleanup = None;
    let result = async {
        // The overlay labels Processing by the last voice mode; without this
        // a dictation Retry would show the previous Ask or Edit session's.
        let _ = app.emit("voice-mode", retry_voice_mode(mode, &source));
        state.set_operation(types::OperationKind::Retry, operation_id);
        emit_stage(
            app,
            state,
            types::AppStage::Retrying,
            "Retrying the saved recording.",
        );
        let retry_path = storage
            .copy_history_audio_for_retry(id)
            .map_err(command_error)?;
        retry_cleanup = Some(TempArtifact::new(retry_path.clone(), false));
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let started = Instant::now();
        let settings = storage.get_settings().map_err(command_error)?;
        // Retry uses the current ASR settings, so it loads the selected model
        // first, as a new recording does; a backend or model change resets it.
        let model_ready =
            ensure_model_loaded_cancellable(app, services, &settings, Some(cancel.clone())).await;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        model_ready?;
        emit_stage(
            app,
            state,
            types::AppStage::Transcribing,
            if settings.asr_backend == "openai-compatible" {
                "Transcribing with the configured speech provider."
            } else {
                "Transcribing locally."
            },
        );
        // History keeps only the captured category, so Retry routes to
        // category-scoped and global dictionary entries and profiles.
        let retry_context = retry_app_context(&source);
        let metrics = storage.metrics_for(mode, source.app_category.as_deref());
        let prompt = storage
            .dictionary_asr_prompt_for(
                crate::app_context::asr_prompt_context(mode, retry_context.as_ref()),
                &settings.asr_backend,
            )
            .unwrap_or_default();
        let asr_started = Instant::now();
        let transcript = services
            .transcriber
            .transcribe_with_locale(
                &retry_path,
                prompt.as_deref(),
                settings.speech_locale.as_deref(),
                cancel.clone(),
            )
            .await;
        if !matches!(transcript, Err(crate::asr::AsrError::Cancelled)) {
            let _ = metrics.add_metric(
                "asr",
                Some(&settings.asr_backend),
                Some(asr_started.elapsed().as_millis() as i64),
                transcript.is_ok(),
                transcript.is_err().then_some("asr_failed"),
            );
        }
        let transcript = transcript.map_err(|error| {
            if crate::commands::TranscriptionFailure::of(&error)
                == crate::commands::TranscriptionFailure::CpuMemory
            {
                crate::commands::CPU_MEMORY_FAILURE.to_owned()
            } else {
                command_error(error)
            }
        })?;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let mut output = transcript.text.clone();
        let mut llm_provider = None;
        let mut correction_failed = false;
        if mode == PipelineMode::Translate {
            let target = source
                .target_language
                .as_deref()
                .ok_or("saved translation target is unavailable")?;
            emit_stage(
                app,
                state,
                types::AppStage::Translating,
                "Translating speech to the selected language.",
            );
            let translation_started = Instant::now();
            let result = correction::translate_transcript(
                &settings,
                &transcript.text,
                target,
                cancel.clone(),
                |_| {},
            )
            .await;
            match classify_ai_stage(result) {
                AiStageResult::Corrected(translated) => {
                    output = translated;
                    llm_provider = Some(settings.correction_provider.as_str());
                    let _ = metrics.add_metric(
                        "voice_translation",
                        llm_provider,
                        Some(translation_started.elapsed().as_millis() as i64),
                        true,
                        None,
                    );
                }
                AiStageResult::Cancelled => return Err("history retry was cancelled".into()),
                AiStageResult::Fallback(error) => {
                    emit_ai_failure(
                        app,
                        "voice_translation_failed",
                        &error,
                        FailureOutcome::UsingTranscript,
                        &correction::failure_message(&error, FailureOutcome::UsingTranscript),
                    );
                    let _ = metrics.add_metric(
                        "voice_translation",
                        Some(settings.correction_provider.as_str()),
                        Some(translation_started.elapsed().as_millis() as i64),
                        false,
                        Some(error.failure_code()),
                    );
                }
            }
        } else if mode == PipelineMode::Edit {
            let selected = source
                .source_text
                .as_deref()
                .ok_or("saved edit source is unavailable")?;
            emit_stage(
                app,
                state,
                types::AppStage::Correcting,
                "Applying the spoken edit instruction.",
            );
            let personalization = super::personalization::resolve(
                storage,
                &settings,
                retry_context.as_ref(),
                &transcript.text,
                Some(selected),
                cancel.clone(),
            )
            .await
            .ok_or("history retry was cancelled")?;
            output = correction::edit_selected_text(
                &settings,
                selected,
                &transcript.text,
                personalization.prompt(),
                cancel.clone(),
                |_| {},
            )
            .await
            .map_err(|error| ai_command_error(app, &error, FailureOutcome::NoAction))?;
            llm_provider = Some(settings.correction_provider.as_str());
        } else if mode == PipelineMode::Ask {
            emit_stage(
                app,
                state,
                types::AppStage::Planning,
                "Choosing a safe Ask action.",
            );
            if source.action_kind.as_deref() == Some("search") {
                let stored_site = stored_search_site(&source)?;
                let personalization = super::personalization::resolve(
                    storage,
                    &settings,
                    retry_context.as_ref(),
                    &transcript.text,
                    None,
                    cancel.clone(),
                )
                .await
                .ok_or("history retry was cancelled")?;
                let plan = correction::generate_ask_plan(
                    &settings,
                    &transcript.text,
                    &personalization.instruction(&ask::planning_prompt(ask::AskContextKind::Caret)),
                    cancel.clone(),
                )
                .await
                .map_err(|error| ai_command_error(app, &error, FailureOutcome::NoAction))?;
                output = ask::validate_fixed_search_retry_plan(
                    &plan,
                    &transcript.text,
                    stored_site,
                    &settings.translation_target_languages,
                )
                .map_err(|_| "Ask search retry plan was blocked".to_string())?;
            } else {
                let action = stored_ask_action(&source)?;
                let input = ask::generation_input(source.source_text.as_deref(), &transcript.text);
                let personalization = super::personalization::resolve(
                    storage,
                    &settings,
                    retry_context.as_ref(),
                    &transcript.text,
                    source.source_text.as_deref(),
                    cancel.clone(),
                )
                .await
                .ok_or("history retry was cancelled")?;
                output = correction::generate_ask_text(
                    &settings,
                    &input,
                    &personalization.instruction(&ask::generation_prompt_with_target(&action)),
                    cancel.clone(),
                )
                .await
                .map_err(|error| ai_command_error(app, &error, FailureOutcome::NoAction))?;
            }
            llm_provider = Some(settings.correction_provider.as_str());
        } else if settings.text_correction_enabled {
            emit_stage(
                app,
                state,
                types::AppStage::Correcting,
                "Correcting the transcript with the configured AI provider.",
            );
            let hints = storage
                .dictionary_correction_terms(&transcript.text, retry_context.as_ref())
                .unwrap_or_default();
            let correction_started = Instant::now();
            let result = correction::correct_transcript(
                &settings,
                &transcript.text,
                &hints,
                personalization::resolve_profile(&settings, retry_context.as_ref())
                    .map(|profile| personalization::guidance(&profile))
                    .as_deref(),
                cancel.clone(),
                |_| {},
            )
            .await;
            match classify_ai_stage(result) {
                AiStageResult::Corrected(corrected) => {
                    output = corrected;
                    llm_provider = Some(settings.correction_provider.as_str());
                }
                AiStageResult::Cancelled => return Err("history retry was cancelled".into()),
                AiStageResult::Fallback(error) => {
                    // Like a new recording, a failed correction keeps the new
                    // transcript instead of failing the whole Retry.
                    correction_failed = true;
                    emit_ai_failure(
                        app,
                        "text_correction_failed",
                        &error,
                        FailureOutcome::UsingTranscript,
                        &correction_failure_status(&error),
                    );
                    let _ = metrics.add_metric(
                        "text_correction",
                        Some(settings.correction_provider.as_str()),
                        Some(correction_started.elapsed().as_millis() as i64),
                        false,
                        Some(correction_failure_metric_code(&error)),
                    );
                }
            }
        }
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let latency_ms = started.elapsed().as_millis() as i64;
        let history_save_status = services
            .lifecycle
            .commit_side_effect(operation_id, || {
                Ok::<HistorySaveStatus, String>(save_retry_result(
                    storage,
                    &NewHistoryItem {
                        transcript_text: &transcript.text,
                        processed_text: (output != transcript.text).then_some(output.as_str()),
                        source_text: source.source_text.as_deref(),
                        instruction_text: if source.mode == "edit" || source.mode == "ask" {
                            Some(transcript.text.as_str())
                        } else {
                            None
                        },
                        action_kind: source.action_kind.as_deref(),
                        search_site: source.search_site.as_deref(),
                        mode: if mode == PipelineMode::Dictate {
                            retry_dictate_history_mode(llm_provider, correction_failed)
                        } else {
                            &source.mode
                        },
                        asr_provider: &transcript.model,
                        llm_provider,
                        target_language: source.target_language.as_deref(),
                        app_category: source.app_category.as_deref(),
                        duration_ms: source.duration_ms,
                        latency_ms: Some(latency_ms),
                        retry_of_id: Some(id),
                        insertion_result: None,
                        insertion_detail: None,
                    },
                    &retry_path,
                    id,
                ))
            })
            .map_err(str::to_string)?
            .map_err(command_error)?;
        // The committed Retry still owns the lifecycle. Publish its completion
        // before releasing it, so a recording started afterwards cannot have
        // its state overwritten by this completion.
        recording_overlay::set_phase(app, &AppPhase::Completed);
        let snapshot = state.complete(
            output.clone(),
            "History retry completed without inserting text.".into(),
        );
        let _ = app.emit("app-state", snapshot);
        services.lifecycle.finish(operation_id);
        report_history_save_warning(app, history_save_status);
        Ok(RecordingResult {
            text: output,
            insertion: "history_only".into(),
            duration_ms: source.duration_ms.unwrap_or_default() as u64,
            latency_ms: latency_ms as u64,
        })
    }
    .await;
    if retry_cleanup
        .as_mut()
        .is_some_and(|artifact: &mut TempArtifact| artifact.cleanup().is_err())
    {
        emit_status(
            app,
            "artifact_cleanup_failed",
            "Temporary retry audio cleanup failed.",
        );
    }
    if result.is_err() {
        let message = if services.lifecycle.is_cancelled(operation_id) {
            "History retry cancelled."
        } else if result
            .as_ref()
            .err()
            .is_some_and(|error| error == crate::commands::CPU_MEMORY_FAILURE)
        {
            crate::commands::CPU_MEMORY_FAILURE
        } else {
            "History retry failed."
        };
        emit_state(
            app,
            state,
            if services.lifecycle.is_cancelled(operation_id) {
                AppPhase::Idle
            } else {
                AppPhase::Error
            },
            message,
        );
    }
    result
}

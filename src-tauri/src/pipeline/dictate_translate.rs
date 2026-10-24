//! Dictate and voice Translate processing and delivery.
use super::{classify_ai_stage, AiStageResult, PipelineContext, RecordedTake};
use crate::commands::{
    cancellation_copy, commit_dictation_output, completion_persistence_warning,
    correction_failure_metric_code, correction_failure_status, emit_correction_preview,
    output_limit_inserted_message, report_clipboard_not_restored, report_temp_cleanup,
    save_history, CommittedOutput, INSERTION_FAILED, TRANSLATION_PASTE_UNVERIFIED,
    TRANSLATION_TARGET_CHANGED,
};
use crate::{
    app_context, command_error, correction,
    correction::FailureOutcome,
    emit_ai_failure, emit_stage, emit_state, emit_status, injection,
    injection::{InsertResult, InsertionDetail},
    personalization, recording_overlay,
    state::PipelineMode,
    types::{self, AppPhase, NewHistoryItem, RecordingResult},
};
use std::time::Instant;
use tauri::{Emitter, Manager};

pub(crate) async fn finish(
    context: PipelineContext<'_>,
    take: RecordedTake,
) -> Result<RecordingResult, String> {
    let PipelineContext {
        app,
        services,
        state,
        storage,
        operation_id,
    } = context;
    let RecordedTake {
        settings,
        transcript,
        artifact,
        mut artifact_cleanup,
        started,
        correction_cancel,
        mode,
        app_context,
        translation_target,
        mut draft,
    } = take;
    let metrics = storage.metrics_for(mode, app_context::history_category(app_context.as_ref()));
    let duration_ms = artifact.duration.as_millis() as u64;
    let (cancel_message, cancel_error) = cancellation_copy(mode);
    let mut draft = draft
        .take()
        .expect("non-edit recording has a live dictation session");
    let mut final_text = transcript.text.clone();
    let mut processed_text = None;
    let mut llm_provider = None;
    let mut correction_failed = false;
    let mut translation_failed = false;
    let mut correction_output_limit = None;
    let _ = app.emit("app-state", state.publish_result(transcript.text.clone()));
    // Full-recording recognition reconciles the last live hypothesis before AI correction.
    draft.monitor.wait_for_shortcut_release().await;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(app, state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    if let Err(error) = draft.update(&transcript.text) {
        emit_status(
            app,
            "streaming_insertion_unavailable",
            &format!("Draft insertion failed; the transcript is available in this app. {error}"),
        );
    }
    let streamed_into_target = draft.pasted;
    if mode == PipelineMode::Translate {
        let Some(target_language) = translation_target.as_deref() else {
            // Startup publishes the target with the session. If it is missing,
            // keep the speech the same way a translation failure does.
            let copied = match commit_dictation_output(&services.lifecycle, operation_id, || {
                draft
                    .injector
                    .copy_to_clipboard(&transcript.text)
                    .map(|()| InsertResult::ClipboardOnly)
            }) {
                CommittedOutput::Ran(copied) => copied.is_ok(),
                // Nothing was copied.
                CommittedOutput::Cancelled => {
                    emit_state(app, state, AppPhase::Idle, cancel_message);
                    return Err(cancel_error.into());
                }
                // Nothing was copied; the phase guard reports the incomplete
                // translation.
                CommittedOutput::Refused(error) => return Err(error.into()),
            };
            emit_correction_preview(app, &transcript.text, "fallback");
            emit_state(
                app,
                state,
                AppPhase::Error,
                if copied {
                    "The translation target language was unavailable; the raw transcript remains on the clipboard."
                } else {
                    "The translation target language was unavailable and the clipboard could not be updated; the raw transcript remains available in this app."
                },
            );
            return Err("translation target language is unavailable".into());
        };
        emit_correction_preview(app, &transcript.text, "draft");
        emit_stage(
            app,
            state,
            types::AppStage::Translating,
            "Translating speech to the selected language.",
        );
        let translation_started = Instant::now();
        llm_provider = Some(settings.correction_provider.clone());
        match classify_ai_stage(
            correction::translate_transcript(
                &settings,
                &transcript.text,
                target_language,
                correction_cancel,
                |delta| emit_correction_preview(app, delta, "streaming"),
            )
            .await,
        ) {
            AiStageResult::Corrected(translated) => {
                final_text = translated;
                processed_text = Some(final_text.clone());
                // Publish before insertion so an insertion error cannot lose
                // the completed translation.
                let _ = app.emit("app-state", state.publish_result(final_text.clone()));
                emit_correction_preview(app, &final_text, "final");
                let _ = metrics.add_metric(
                    "voice_translation",
                    Some(settings.correction_provider.as_str()),
                    Some(translation_started.elapsed().as_millis() as i64),
                    true,
                    None,
                );
            }
            AiStageResult::Cancelled => {
                emit_state(app, state, AppPhase::Idle, "Translation cancelled.");
                return Err("translation was cancelled".into());
            }
            AiStageResult::Fallback(error) => {
                translation_failed = true;
                emit_correction_preview(app, &transcript.text, "fallback");
                let _ = metrics.add_metric(
                    "voice_translation",
                    Some(settings.correction_provider.as_str()),
                    Some(translation_started.elapsed().as_millis() as i64),
                    false,
                    Some(error.failure_code()),
                );
                emit_ai_failure(
                    app,
                    "voice_translation_failed",
                    &error,
                    FailureOutcome::UsingTranscript,
                    &correction::failure_message(&error, FailureOutcome::UsingTranscript),
                );
            }
        }
    } else if settings.text_correction_enabled {
        let correction_hints = storage
            .dictionary_correction_terms(&transcript.text, app_context.as_ref())
            .unwrap_or_default();
        emit_correction_preview(app, &transcript.text, "draft");
        if streamed_into_target {
            emit_state(
                app,
                state,
                AppPhase::Injecting,
                "Inserting the provisional transcript into the captured target.",
            );
        }
        emit_stage(
            app,
            state,
            types::AppStage::Correcting,
            "Correcting the transcript with the configured AI provider.",
        );
        let correction_started = Instant::now();
        match correction::correct_transcript(
            &settings,
            &transcript.text,
            &correction_hints,
            personalization::resolve_profile(&settings, app_context.as_ref())
                .map(|profile| personalization::guidance(&profile))
                .as_deref(),
            correction_cancel,
            |delta| {
                emit_correction_preview(app, delta, "streaming");
            },
        )
        .await
        {
            Ok(corrected) => {
                final_text = corrected;
                emit_correction_preview(app, &final_text, "final");
                processed_text = Some(final_text.clone());
                llm_provider = Some(settings.correction_provider.clone());
                let _ = metrics.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_started.elapsed().as_millis() as i64),
                    true,
                    None,
                );
            }
            Err(correction::CorrectionError::Cancelled) => {
                emit_state(app, state, AppPhase::Idle, "Dictation cancelled.");
                return Err("dictation was cancelled".into());
            }
            Err(error) => {
                correction_failed = true;
                correction_output_limit = output_limit_inserted_message(&error);
                emit_correction_preview(app, &transcript.text, "fallback");
                let _ = metrics.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_started.elapsed().as_millis() as i64),
                    false,
                    Some(correction_failure_metric_code(&error)),
                );
                emit_ai_failure(
                    app,
                    "text_correction_failed",
                    &error,
                    FailureOutcome::UsingTranscript,
                    &correction_failure_status(&error),
                );
            }
        }
    } else {
        emit_correction_preview(app, &transcript.text, "final");
    }
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(app, state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }

    emit_stage(
        app,
        state,
        if streamed_into_target {
            types::AppStage::UpdatingText
        } else {
            types::AppStage::Inserting
        },
        if streamed_into_target {
            "Finalizing the provisional text in the captured target."
        } else {
            "Inserting into the captured target."
        },
    );
    let cancel = services.lifecycle.cancellation(operation_id)?;
    // Settling awaits, so it runs before the commit, which must not.
    if !translation_failed {
        draft.settle_before_final(&cancel).await;
    }
    // A cancel either wins before the paste or copy, or finds the dictation
    // committed and changes nothing.
    let app_for_insertion = app.clone();
    let text_for_insertion = final_text.clone();
    let (draft, committed) = tokio::task::spawn_blocking(move || {
        let services = app_for_insertion.state::<crate::Services>();
        let committed = commit_dictation_output(&services.lifecycle, operation_id, || {
            if translation_failed {
                draft
                    .injector
                    .copy_to_clipboard(&text_for_insertion)
                    .map(|()| InsertResult::ClipboardOnly)
            } else {
                draft.finish(&text_for_insertion, &cancel)
            }
        });
        (draft, committed)
    })
    .await
    .map_err(|_| "dictation insertion task failed".to_owned())?;
    let insertion_result = match committed {
        CommittedOutput::Ran(inserted) => inserted,
        // Nothing was pasted or copied; ends below as a cancelled insertion,
        // whose dropped draft removes its provisional text.
        CommittedOutput::Cancelled => Err(injection::InjectionError::Cancelled),
        // Nothing was pasted or copied; the phase guard reports the
        // incomplete dictation.
        CommittedOutput::Refused(error) => return Err(error.into()),
    };
    // Diagnose before the draft stops its input monitor. Fixed codes only.
    let clipboard_lost = draft.injector.take_clipboard_not_restored();
    let insertion_detail = match &insertion_result {
        Ok(_) if translation_failed => Some("translation_failed"),
        Ok(result) => draft
            .diagnose(*result)
            .or(InsertionDetail::for_confirmed_paste(
                *result,
                clipboard_lost,
            ))
            .map(InsertionDetail::code),
        Err(error) => InsertionDetail::from_error(error).map(InsertionDetail::code),
    };
    // Insertion has returned; helper shutdown and persistence are not insertion.
    // This only hides the overlay. The result below still determines success.
    recording_overlay::set_phase(app, &AppPhase::Completed);
    // The helper observes input only while a provisional replacement session
    // can still mutate the target. Stop it before persisting the result.
    drop(draft);
    if insertion_result == Err(injection::InjectionError::Cancelled) {
        emit_state(app, state, AppPhase::Idle, cancel_message);
        if clipboard_lost {
            report_clipboard_not_restored(app);
        }
        return Err(cancel_error.into());
    }
    let latency_ms = started.elapsed().as_millis() as u64;
    let insertion_label = match insertion_result {
        Err(_) => "insertion_failed",
        Ok(InsertResult::ClipboardPaste) if streamed_into_target => "provisional_replace",
        Ok(InsertResult::ClipboardPaste) => "clipboard_paste",
        Ok(InsertResult::ClipboardOnly) => "clipboard_only",
        Ok(InsertResult::PasteUnverified) => "paste_unverified",
    };
    let inserted = insertion_result == Ok(InsertResult::ClipboardPaste);
    let history_save_status = save_history(
        storage,
        &NewHistoryItem {
            transcript_text: &transcript.text,
            processed_text: processed_text.as_deref(),
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: if mode == PipelineMode::Translate {
                "translate"
            } else if processed_text.is_some() {
                "ai_corrected"
            } else if correction_failed {
                "faithful_fallback"
            } else {
                "faithful"
            },
            asr_provider: &transcript.model,
            llm_provider: llm_provider.as_deref(),
            target_language: translation_target.as_deref(),
            app_category: app_context::history_category(app_context.as_ref()),
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
            insertion_result: Some(insertion_label),
            insertion_detail,
        },
        Some(&artifact.path),
    );
    // Only an AI correction of a Dictate transcript proposes a spelling; a
    // translation is a different text, not a correction of the transcript.
    if mode == PipelineMode::Dictate && processed_text.is_some() {
        let _ =
            storage.add_dictionary_candidate_from_correction(&transcript.text, &final_text, None);
    }
    let mode_metric = if mode == PipelineMode::Translate {
        "voice_translate"
    } else {
        "dictation"
    };
    let metric_result = metrics.add_metric(
        mode_metric,
        Some(&settings.asr_backend),
        Some(latency_ms as i64),
        !translation_failed,
        translation_failed.then_some("translation_failed"),
    );
    // The pipeline metric above records recognition; this one records whether
    // the text reached the target, as `outcome` or `outcome:reason` codes.
    let insertion_code = insertion_detail.map_or_else(
        || insertion_label.to_owned(),
        |detail| format!("{insertion_label}:{detail}"),
    );
    let insertion_metric = metrics.add_metric(
        "insertion",
        Some(mode_metric),
        Some(latency_ms as i64),
        inserted,
        (!inserted).then_some(insertion_code.as_str()),
    );
    let metric_failed = metric_result.is_err() || insertion_metric.is_err();
    let insertion = match insertion_result {
        Ok(insertion) => insertion,
        Err(error) => {
            // The clipboard is not overwritten after an insertion error (it
            // may hold data that could not be preserved); keep the text here.
            let _ = app.emit("app-state", state.publish_result(final_text.clone()));
            emit_state(app, state, AppPhase::Error, INSERTION_FAILED);
            if let Some((kind, message)) =
                completion_persistence_warning(history_save_status, metric_failed)
            {
                emit_status(app, kind, message);
            }
            recording_overlay::show_insertion_notice(
                app,
                injection::notice_outcome(insertion_label, clipboard_lost),
                insertion_detail,
            );
            report_temp_cleanup(app, &mut artifact_cleanup);
            return Err(command_error(error));
        }
    };
    let completion = if translation_failed {
        "Translation failed; the raw transcript remains on the clipboard."
    } else if mode == PipelineMode::Translate && insertion == InsertResult::PasteUnverified {
        TRANSLATION_PASTE_UNVERIFIED
    } else if mode == PipelineMode::Translate && insertion == InsertResult::ClipboardOnly {
        TRANSLATION_TARGET_CHANGED
    } else if mode == PipelineMode::Translate {
        "Voice translation inserted."
    } else if insertion == InsertResult::PasteUnverified && streamed_into_target {
        // The clipboard may still hold the first live fragment, not the result.
        "The live draft could not be confirmed, so it was not replaced; the final result is available in this app."
    } else if insertion == InsertResult::PasteUnverified {
        "Pasted. This field cannot confirm the result; press Ctrl+V only if the text did not appear (it is on the clipboard)."
    } else if insertion == InsertResult::ClipboardOnly && streamed_into_target {
        "The provisional text could not be safely replaced; the final result remains on the clipboard."
    } else if insertion == InsertResult::ClipboardOnly {
        "Automatic insertion failed; the result remains on the clipboard."
    } else if let Some(message) = correction_output_limit {
        message
    } else if correction_failed {
        "AI correction failed; the original transcript was inserted."
    } else {
        "Dictation inserted successfully."
    };
    let snapshot = state.complete(final_text.clone(), completion.into());
    let _ = app.emit("app-state", snapshot);
    emit_status(app, insertion_label, completion);
    if let Some((kind, message)) =
        completion_persistence_warning(history_save_status, metric_failed)
    {
        emit_status(app, kind, message);
    }
    if !inserted {
        // With opt-in live insertion the raw draft may already be in the field.
        let notice = match insertion {
            InsertResult::ClipboardOnly if streamed_into_target => "draft_kept",
            InsertResult::PasteUnverified if streamed_into_target => "draft_unverified",
            _ => insertion_label,
        };
        let notice = injection::notice_outcome(notice, clipboard_lost);
        recording_overlay::show_insertion_notice(app, notice, insertion_detail);
    } else if clipboard_lost {
        // A confirmed paste that could not restore the previous clipboard.
        let notice = injection::notice_outcome(insertion_label, true);
        recording_overlay::show_insertion_notice(app, notice, None);
    } else if correction_failed {
        // The text arrived uncorrected; say why where the user is looking.
        recording_overlay::show_message_notice(
            app,
            recording_overlay::NoticeSeverity::Warning,
            completion,
        );
    }
    report_temp_cleanup(app, &mut artifact_cleanup);
    Ok(RecordingResult {
        text: final_text,
        insertion: insertion_label.into(),
        duration_ms,
        latency_ms,
    })
}

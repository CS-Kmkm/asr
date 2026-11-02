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

/// Shown when the final paste is held back because the original field lost
/// focus or its IME is still composing. Fixed copy: no transcript or title.
const TARGET_WAIT_HINT: &str =
    "To insert, return to the original field or confirm the IME conversion.";

pub(crate) async fn finish(
    context: PipelineContext<'_>,
    take: RecordedTake,
) -> Result<RecordingResult, String> {
    let app = context.app.clone();
    let operation_id = context.operation_id;
    // The final raw-hypothesis update and every early-return draft drop can
    // inspect UIA or restore a clipboard. Keep this whole owner off runtime
    // workers, including while it awaits optional AI correction.
    crate::live_dictation::spawn_recording_owner(async move {
        let services = app.state::<crate::Services>();
        let state = app.state::<crate::state::AppState>();
        let storage = app.state::<crate::storage::Storage>();
        finish_owned(
            PipelineContext {
                app: &app,
                services: &services,
                state: &state,
                storage: &storage,
                operation_id,
            },
            take,
        )
        .await
    })
    .await
    .map_err(|_| "dictation finalization task failed".to_owned())?
}

async fn finish_owned(
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
        automatic,
        settings,
        transcript,
        artifact,
        artifact_cleanup,
        started,
        correction_cancel,
        mode,
        app_context,
        translation_target,
        mut draft,
        // Recorded by the shared stop pipeline after delivery.
        stop_timings: _,
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
    // Time to the first nonempty provider callback. This overlaps the total AI
    // stage; it does not isolate connection setup or server queueing.
    let mut first_output_ms = None;
    let _ = app.emit("app-state", state.publish_result(transcript.text.clone()));
    // Full-recording recognition reconciles the last live hypothesis before AI
    // correction. Only a live draft edits the target here; a deferred final
    // paste waits for released keys while settling, so AI need not wait.
    if draft.edits_target_live() {
        draft.monitor.wait_for_shortcut_release().await;
    }
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
        let result = correction::translate_voice_request(
            &settings,
            &transcript.text,
            target_language,
            automatic,
            correction_cancel,
            |delta| {
                note_first_output(&mut first_output_ms, translation_started, delta);
                emit_correction_preview(app, delta, "streaming");
            },
        )
        .await;
        let translation_ms = translation_started.elapsed().as_millis() as i64;
        // A failed or cancelled request has no comparable first output.
        first_output_ms = first_output_ms.filter(|_| result.is_ok());
        match classify_ai_stage(result) {
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
                    Some(translation_ms),
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
                    Some(translation_ms),
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
        let result = correction::correct_transcript(
            &settings,
            &transcript.text,
            &correction_hints,
            personalization::resolve_profile(&settings, app_context.as_ref())
                .map(|profile| personalization::guidance(&profile))
                .as_deref(),
            correction_cancel,
            |delta| {
                note_first_output(&mut first_output_ms, correction_started, delta);
                emit_correction_preview(app, delta, "streaming");
            },
        )
        .await;
        let correction_ms = correction_started.elapsed().as_millis() as i64;
        first_output_ms = first_output_ms.filter(|_| result.is_ok());
        match result {
            Ok(corrected) => {
                final_text = corrected;
                emit_correction_preview(app, &final_text, "final");
                processed_text = Some(final_text.clone());
                llm_provider = Some(settings.correction_provider.clone());
                let _ = metrics.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_ms),
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
                    Some(correction_ms),
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
    // A cancel either wins before the paste or copy, or finds the dictation
    // committed and changes nothing.
    let app_for_insertion = app.clone();
    let text_for_insertion = final_text.clone();
    let runtime = tokio::runtime::Handle::current();
    let (committed, clipboard_lost, insertion_detail, settle_ms, paste_ms) =
        tokio::task::spawn_blocking(move || {
            let services = app_for_insertion.state::<crate::Services>();
            // Readiness may synchronously query UIA; await quiet input on this
            // blocking owner before taking the irreversible commit fence.
            let settle_started = Instant::now();
            // Only a real wait for quiet input is timed; most takes need none.
            let settle_ms = (!translation_failed
                && runtime.block_on(draft.settle_before_final_with_hint(&cancel, || {
                    recording_overlay::show_live_notice(&app_for_insertion, TARGET_WAIT_HINT);
                })))
            .then(|| settle_started.elapsed().as_millis() as i64);
            let paste_started = Instant::now();
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
            let paste_ms = paste_started.elapsed().as_millis() as i64;
            let clipboard_lost = draft.injector.take_clipboard_not_restored();
            let insertion_detail = match &committed {
                CommittedOutput::Ran(Ok(_)) if translation_failed => Some("translation_failed"),
                CommittedOutput::Ran(Ok(result)) => draft
                    .diagnose(*result)
                    .or(InsertionDetail::for_confirmed_paste(
                        *result,
                        clipboard_lost,
                    ))
                    .map(InsertionDetail::code),
                CommittedOutput::Ran(Err(error)) => {
                    InsertionDetail::from_error(error).map(InsertionDetail::code)
                }
                _ => None,
            };
            recording_overlay::set_phase(&app_for_insertion, &AppPhase::Completed);
            drop(draft);
            (
                committed,
                clipboard_lost,
                insertion_detail,
                settle_ms,
                paste_ms,
            )
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
    // Reserved before the History save so a Delete all that starts from here
    // on also waits for this take's deferred metrics.
    let completion_slot = storage.reserve_completion();
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
    // History and dictionary candidates stay above: the History and
    // Dictionary pages re-read them when the completed phase is published.
    // Metrics and temporary-audio cleanup are deferred past the lifecycle
    // guard, in take order, and reported as before once they ran.
    let deferred = DeferredCompletion {
        app: app.clone(),
        storage: storage.clone(),
        app_context: app_context.clone(),
        metrics: TakeMetrics {
            mode,
            asr_backend: settings.asr_backend.clone(),
            correction_provider: settings.correction_provider.clone(),
            latency_ms: latency_ms as i64,
            translation_failed,
            insertion_label,
            insertion_detail,
            inserted,
            first_output_ms,
            settle_ms,
            paste_ms,
        },
        history_save_status,
        artifact_cleanup,
        reservation: completion_slot,
    };
    let insertion = match insertion_result {
        Ok(insertion) => insertion,
        Err(error) => {
            // The clipboard is not overwritten after an insertion error (it
            // may hold data that could not be preserved); keep the text here.
            let _ = app.emit("app-state", state.publish_result(final_text.clone()));
            emit_state(app, state, AppPhase::Error, INSERTION_FAILED);
            recording_overlay::show_insertion_notice(
                app,
                injection::notice_outcome(insertion_label, clipboard_lost),
                insertion_detail,
            );
            deferred.queue();
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
    deferred.queue();
    Ok(RecordingResult {
        text: final_text,
        insertion: insertion_label.into(),
        duration_ms,
        latency_ms,
    })
}

/// Remembers when the provider's first non-empty streamed text arrived.
fn note_first_output(first_output_ms: &mut Option<i64>, started: Instant, delta: &str) {
    if first_output_ms.is_none() && !delta.is_empty() {
        *first_output_ms = Some(started.elapsed().as_millis() as i64);
    }
}

/// The usage metrics of one delivered take, all measured before delivery
/// ended; only their writes are deferred.
struct TakeMetrics {
    mode: PipelineMode,
    asr_backend: String,
    correction_provider: String,
    latency_ms: i64,
    translation_failed: bool,
    insertion_label: &'static str,
    insertion_detail: Option<&'static str>,
    inserted: bool,
    first_output_ms: Option<i64>,
    settle_ms: Option<i64>,
    paste_ms: i64,
}

impl TakeMetrics {
    /// Writes the metrics; returns whether a mode or insertion record failed.
    fn record(&self, metrics: crate::storage::MetricRecorder<'_>) -> bool {
        let translate = self.mode == PipelineMode::Translate;
        let mode_metric = if translate {
            "voice_translate"
        } else {
            "dictation"
        };
        let metric_result = metrics.add_metric(
            mode_metric,
            Some(&self.asr_backend),
            Some(self.latency_ms),
            !self.translation_failed,
            self.translation_failed.then_some("translation_failed"),
        );
        // The pipeline metric above records recognition; this one records
        // whether the text reached the target, as `outcome` or
        // `outcome:reason` codes.
        let insertion_code = self.insertion_detail.map_or_else(
            || self.insertion_label.to_owned(),
            |detail| format!("{}:{detail}", self.insertion_label),
        );
        let insertion_metric = metrics.add_metric(
            "insertion",
            Some(mode_metric),
            Some(self.latency_ms),
            self.inserted,
            (!self.inserted).then_some(insertion_code.as_str()),
        );
        record_first_output(
            metrics,
            if translate {
                "voice_translation_first_output"
            } else {
                "text_correction_first_output"
            },
            &self.correction_provider,
            self.first_output_ms,
        );
        if let Some(settle_ms) = self.settle_ms {
            let _ = metrics.add_metric(
                "insertion_settle",
                Some(mode_metric),
                Some(settle_ms),
                true,
                None,
            );
        }
        let _ = metrics.add_metric(
            "insertion_paste",
            Some(mode_metric),
            Some(self.paste_ms),
            self.inserted,
            None,
        );
        metric_result.is_err() || insertion_metric.is_err()
    }
}

/// What a delivered take still writes and removes after it is shown as
/// complete. The completion job owns it, including the temporary capture,
/// and runs after the lifecycle guard is released, in take order.
struct DeferredCompletion {
    app: tauri::AppHandle,
    storage: crate::storage::Storage,
    app_context: Option<types::AppContext>,
    metrics: TakeMetrics,
    history_save_status: crate::pipeline::finish::HistorySaveStatus,
    artifact_cleanup: crate::TempArtifact,
    reservation: crate::storage::CompletionReservation,
}

impl DeferredCompletion {
    fn queue(self) {
        let (reservation, job) = self.into_job();
        reservation.defer(move || job.run());
    }

    fn into_job(self) -> (crate::storage::CompletionReservation, CompletionJob) {
        let Self {
            app,
            storage,
            app_context,
            metrics,
            history_save_status,
            artifact_cleanup,
            reservation,
        } = self;
        (
            reservation,
            CompletionJob {
                app,
                storage,
                app_context,
                metrics,
                history_save_status,
                artifact_cleanup,
            },
        )
    }
}

/// The part of a [`DeferredCompletion`] that runs on the completion queue.
struct CompletionJob {
    app: tauri::AppHandle,
    storage: crate::storage::Storage,
    app_context: Option<types::AppContext>,
    metrics: TakeMetrics,
    history_save_status: crate::pipeline::finish::HistorySaveStatus,
    artifact_cleanup: crate::TempArtifact,
}

impl CompletionJob {
    /// Writes the metrics, reports a persistence failure with the same
    /// status as before, then removes the temporary capture.
    fn run(mut self) {
        let metric_failed = self.metrics.record(self.storage.metrics_for(
            self.metrics.mode,
            app_context::history_category(self.app_context.as_ref()),
        ));
        if let Some((kind, message)) =
            completion_persistence_warning(self.history_save_status, metric_failed)
        {
            emit_status(&self.app, kind, message);
        }
        report_temp_cleanup(&self.app, &mut self.artifact_cleanup);
    }
}

fn record_first_output(
    metrics: crate::storage::MetricRecorder<'_>,
    event_type: &str,
    provider: &str,
    first_output_ms: Option<i64>,
) {
    if let Some(first_output_ms) = first_output_ms {
        let _ = metrics.add_metric(
            event_type,
            Some(provider),
            Some(first_output_ms),
            true,
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn first_output_ignores_empty_updates_and_preserves_the_first_observation() {
        let started = Instant::now();
        let mut first = None;
        note_first_output(&mut first, started, "");
        assert_eq!(first, None);

        note_first_output(&mut first, started, "first");
        let observed = first.expect("nonempty callback has a timing");
        // An artificially old start would produce a different time if a later
        // callback overwrote the first one; no wall-clock sleeps are needed.
        note_first_output(&mut first, started - Duration::from_secs(60), "later");
        note_first_output(&mut first, started, "");
        assert_eq!(first, Some(observed));
    }

    fn take_metrics(latency_ms: i64) -> TakeMetrics {
        TakeMetrics {
            mode: PipelineMode::Dictate,
            asr_backend: "faster-whisper".into(),
            correction_provider: "openai".into(),
            latency_ms,
            translation_failed: false,
            insertion_label: "clipboard_only",
            insertion_detail: Some("target_changed"),
            inserted: false,
            first_output_ms: Some(5),
            settle_ms: None,
            paste_ms: 7,
        }
    }

    #[test]
    fn deferred_take_metrics_keep_their_records_and_take_order() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        for latency_ms in [100, 200, 300] {
            let job_storage = storage.clone();
            let metrics = take_metrics(latency_ms);
            storage.defer_completion(move || {
                assert!(!metrics.record(job_storage.metrics_for(PipelineMode::Dictate, None)));
            });
        }
        assert!(storage.drain_deferred_completions(Duration::from_secs(10)));
        let report = storage.diagnostics().unwrap();
        let summary = |event: &str| {
            report
                .summaries
                .iter()
                .find(|row| row.event_type == event)
                .unwrap_or_else(|| panic!("missing {event}"))
        };
        assert_eq!(summary("dictation").count, 3);
        assert_eq!(summary("dictation").p50_ms, Some(200));
        assert_eq!(summary("insertion").successes, 0);
        assert_eq!(summary("text_correction_first_output").count, 3);
        assert_eq!(summary("insertion_paste").count, 3);
        assert!(report
            .summaries
            .iter()
            .all(|row| row.event_type != "insertion_settle"));
        assert!(report
            .outcomes
            .iter()
            .any(|row| row.event_type == "insertion"
                && row.code.as_deref() == Some("clipboard_only:target_changed")
                && row.count == 3));
        // Take order: each take's records follow the previous take's.
        assert_eq!(
            storage.metric_durations_for_test("dictation"),
            vec![100, 200, 300]
        );
    }

    #[test]
    fn first_output_records_zero_but_does_not_invent_a_sample_without_output() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        let metrics = storage.metrics_for(PipelineMode::Dictate, None);
        record_first_output(metrics, "text_correction_first_output", "openai", None);
        assert!(storage.diagnostics().unwrap().summaries.is_empty());

        let mut first = Some(0);
        note_first_output(&mut first, Instant::now(), "later");
        record_first_output(metrics, "text_correction_first_output", "openai", first);
        let report = storage.diagnostics().unwrap();
        assert_eq!(report.summaries.len(), 1);
        assert_eq!(report.summaries[0].timed_count, 1);
        assert_eq!(report.summaries[0].p50_ms, Some(0));
    }
}

//! Dictate and voice Translate processing and delivery.
use super::{classify_ai_stage, AiStageResult, PipelineContext, RecordedTake};
use crate::commands::{
    cancellation_copy, commit_dictation_output, completion_persistence_warning,
    correction_failure_metric_code, correction_failure_status, emit_correction_preview,
    output_limit_inserted_message, report_clipboard_not_restored, CommittedOutput,
    INSERTION_FAILED, TRANSLATION_PASTE_UNVERIFIED, TRANSLATION_TARGET_CHANGED,
};
use crate::pipeline::finish::{
    save_history_reporting_row, DICTIONARY_CHANGED_EVENT, HISTORY_CHANGED_EVENT,
    TEMP_CLEANUP_FAILED,
};
use crate::{
    app_context, command_error, correction,
    correction::FailureOutcome,
    emit_ai_failure, emit_stage, emit_state, emit_status, injection,
    injection::{InsertResult, InsertionDetail},
    recording_overlay,
    state::PipelineMode,
    storage::{TakeOutcome, TakeSpan, TakeStage},
    types::{self, AppPhase, NewHistoryItem, RecordingResult},
};
use std::time::Instant;
use tauri::{Emitter, Manager};

/// A Smart Dictate/Edit take whose selection could not be read at the press.
const SMART_COPY_ONLY: &str = "The selection could not be checked, so the result was copied instead of inserted. Paste it with Ctrl+V.";

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
        mut speculation,
        // Recorded by the shared stop pipeline after delivery.
        stop_timings: _,
        trace,
        smart_route,
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
        let release_started = Instant::now();
        draft.monitor.wait_for_shortcut_release().await;
        trace.span_since(TakeSpan::ShortcutRelease, release_started);
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
        trace.span(TakeSpan::Ai, translation_ms);
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
        let correction_hints =
            super::speculative::correction_hints(storage, &transcript.text, app_context.as_ref());
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
        let style_guidance = super::speculative::style_guidance(&settings, app_context.as_ref());
        // An opt-in speculative correction of this exact text with these
        // exact inputs replaces the request; on a hit the per-take AI span
        // and first-output span measure only the wait from here. Any other
        // speculation is cancelled here, never awaited.
        let adopted = match speculation.take() {
            Some(speculation) => {
                speculation
                    .adopt(
                        &transcript.text,
                        &settings,
                        &correction_hints,
                        style_guidance.as_deref(),
                        correction_started,
                        metrics,
                    )
                    .await
            }
            None => None,
        };
        let speculative_hit = adopted.is_some();
        let result = match adopted {
            Some(adopted) => {
                // Span only: a clamped wait is not comparable with the
                // `text_correction_first_output` metric of full requests.
                if let Some(first_output_ms) = adopted.first_output_ms {
                    trace.span(TakeSpan::AiFirstOutput, first_output_ms);
                }
                Ok(adopted.text)
            }
            None => {
                correction::correct_transcript(
                    &settings,
                    &transcript.text,
                    &correction_hints,
                    style_guidance.as_deref(),
                    correction_cancel,
                    |delta| {
                        note_first_output(&mut first_output_ms, correction_started, delta);
                        emit_correction_preview(app, delta, "streaming");
                    },
                )
                .await
            }
        };
        let correction_ms = correction_started.elapsed().as_millis() as i64;
        trace.span(TakeSpan::Ai, correction_ms);
        first_output_ms = first_output_ms.filter(|_| result.is_ok());
        match result {
            Ok(corrected) => {
                final_text = corrected;
                emit_correction_preview(app, &final_text, "final");
                processed_text = Some(final_text.clone());
                llm_provider = Some(settings.correction_provider.clone());
                record_text_correction_success(
                    metrics,
                    &settings.correction_provider,
                    correction_ms,
                    speculative_hit,
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
    // Any speculation not decided above (none should remain) is cancelled.
    drop(speculation);
    if let Some(first_output_ms) = first_output_ms {
        trace.span(TakeSpan::AiFirstOutput, first_output_ms);
    }
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(app, state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    trace.stage(TakeStage::Delivery);

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
    let trace_for_insertion = trace.clone();
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
            let trace = trace_for_insertion;
            if let Some(settle_ms) = settle_ms {
                trace.span(TakeSpan::Settle, settle_ms);
            }
            if let CommittedOutput::Ran(inserted) = &committed {
                trace.span(TakeSpan::Paste, paste_ms);
                if inserted.is_ok() {
                    trace.delivered();
                }
            }
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
            // Dropping the draft stops its input-monitor helper.
            let helper_exit_started = Instant::now();
            drop(draft);
            trace.span_since(TakeSpan::HelperExit, helper_exit_started);
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
    let insertion_detail = crate::smart_dictate::copy_only_detail(smart_route, insertion_detail);
    let copy_only = matches!(
        smart_route,
        Some(crate::smart_dictate::SmartRoute::CopyOnly(_))
    );
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
    // Reserved before the take is shown as complete, so a Delete all or a
    // retention change that starts from here on waits for this take's
    // deferred History row, dictionary candidate and metrics.
    let completion_slot = storage.reserve_completion();
    // Only an AI correction of a Dictate transcript proposes a spelling; a
    // translation is a different text, not a correction of the transcript.
    let dictionary_candidate = (mode == PipelineMode::Dictate && processed_text.is_some())
        .then(|| (transcript.text.clone(), final_text.clone()));
    // History, the dictionary candidate, metrics and temporary-audio cleanup
    // are deferred past the lifecycle guard, in take order; the History and
    // Dictionary pages re-read on the job's change events.
    let deferred = DeferredCompletion {
        app: app.clone(),
        job: CompletionJob {
            storage: storage.clone(),
            app_context: app_context.clone(),
            history: DeferredHistory {
                mode: if mode == PipelineMode::Translate {
                    "translate"
                } else if processed_text.is_some() {
                    "ai_corrected"
                } else if correction_failed {
                    "faithful_fallback"
                } else {
                    "faithful"
                },
                transcript_text: transcript.text,
                processed_text,
                asr_provider: transcript.model,
                llm_provider,
                target_language: translation_target,
                duration_ms: duration_ms as i64,
                latency_ms: latency_ms as i64,
                insertion_label,
                insertion_detail,
                audio_path: artifact.path.clone(),
            },
            dictionary_candidate,
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
            artifact_cleanup,
            trace: trace.clone(),
        },
        reservation: completion_slot,
    };
    let insertion = match insertion_result {
        Ok(insertion) => insertion,
        Err(error) => {
            trace.outcome(TakeOutcome::InsertionFailed);
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
    } else if insertion == InsertResult::ClipboardOnly && copy_only {
        SMART_COPY_ONLY
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

/// Records a successful Dictate correction. An adopted speculative result
/// counts but is untimed, so `text_correction` percentiles stay those of
/// full provider requests (its full duration is in `speculative_correction`).
fn record_text_correction_success(
    metrics: crate::storage::MetricRecorder<'_>,
    provider: &str,
    correction_ms: i64,
    speculative_hit: bool,
) {
    let _ = metrics.add_metric(
        "text_correction",
        Some(provider),
        (!speculative_hit).then_some(correction_ms),
        true,
        None,
    );
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

/// The History row of a delivered take, owned so the completion job can
/// write it after the take is shown as complete.
struct DeferredHistory {
    transcript_text: String,
    processed_text: Option<String>,
    mode: &'static str,
    asr_provider: String,
    llm_provider: Option<String>,
    target_language: Option<String>,
    duration_ms: i64,
    latency_ms: i64,
    insertion_label: &'static str,
    insertion_detail: Option<&'static str>,
    /// The temporary capture the retained recording is copied from. The
    /// job's `artifact_cleanup` owns it and removes it only after the copy.
    audio_path: std::path::PathBuf,
}

impl DeferredHistory {
    fn item<'a>(&'a self, app_category: Option<&'a str>) -> NewHistoryItem<'a> {
        NewHistoryItem {
            transcript_text: &self.transcript_text,
            processed_text: self.processed_text.as_deref(),
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: self.mode,
            asr_provider: &self.asr_provider,
            llm_provider: self.llm_provider.as_deref(),
            target_language: self.target_language.as_deref(),
            app_category,
            duration_ms: Some(self.duration_ms),
            latency_ms: Some(self.latency_ms),
            retry_of_id: None,
            insertion_result: Some(self.insertion_label),
            insertion_detail: self.insertion_detail,
        }
    }
}

/// What a delivered take still writes and removes after it is shown as
/// complete, and the app its job reports to.
struct DeferredCompletion {
    app: tauri::AppHandle,
    job: CompletionJob,
    reservation: crate::storage::CompletionReservation,
}

impl DeferredCompletion {
    fn queue(self) {
        let Self {
            app,
            job,
            reservation,
        } = self;
        reservation.defer(move || job.run(&mut |notice| notice.deliver(&app)));
    }
}

/// What the completion job tells the main window, in the order it happens.
/// The change events carry no content; the statuses keep their copy.
#[derive(Debug, PartialEq, Eq)]
enum CompletionNotice {
    HistoryChanged,
    DictionaryChanged,
    /// History or metrics could not be (fully) saved.
    PersistenceFailed {
        history: crate::pipeline::finish::HistorySaveStatus,
        metric_failed: bool,
    },
    TempCleanupFailed,
}

impl CompletionNotice {
    fn deliver(self, app: &tauri::AppHandle) {
        match self {
            Self::HistoryChanged => {
                let _ = app.emit_to("main", HISTORY_CHANGED_EVENT, ());
            }
            Self::DictionaryChanged => {
                let _ = app.emit_to("main", DICTIONARY_CHANGED_EVENT, ());
            }
            Self::PersistenceFailed {
                history,
                metric_failed,
            } => {
                if let Some((kind, message)) =
                    completion_persistence_warning(history, metric_failed)
                {
                    emit_status(app, kind, message);
                }
            }
            Self::TempCleanupFailed => {
                let (kind, message) = TEMP_CLEANUP_FAILED;
                emit_status(app, kind, message);
            }
        }
    }
}

/// The deferred part of a delivered take. It owns the temporary capture and
/// runs after the lifecycle guard is released, after every job reserved
/// before it; Delete all and retention changes wait for it.
struct CompletionJob {
    storage: crate::storage::Storage,
    app_context: Option<types::AppContext>,
    history: DeferredHistory,
    /// The transcript and its AI correction, when they may propose one.
    dictionary_candidate: Option<(String, String)>,
    metrics: TakeMetrics,
    artifact_cleanup: crate::TempArtifact,
    trace: super::take_trace::TakeTrace,
}

impl CompletionJob {
    /// Saves History (copying the recording it retains), then a dictionary
    /// candidate, then the metrics; reports a persistence failure with the
    /// same status as before; and only then removes the temporary capture.
    fn run(mut self, notify: &mut impl FnMut(CompletionNotice)) {
        let job_started = Instant::now();
        let app_category = app_context::history_category(self.app_context.as_ref());
        let history_save_started = Instant::now();
        let (history_save_status, history_saved) = save_history_reporting_row(
            &self.storage,
            &self.history.item(app_category),
            Some(&self.history.audio_path),
        );
        self.trace
            .span_since(TakeSpan::HistorySave, history_save_started);
        if history_saved {
            notify(CompletionNotice::HistoryChanged);
        }
        if let Some((original, corrected)) = &self.dictionary_candidate {
            if let Ok(Some(_)) = self
                .storage
                .add_dictionary_candidate_from_correction(original, corrected, None)
            {
                notify(CompletionNotice::DictionaryChanged);
            }
        }
        let metric_failed = self
            .metrics
            .record(self.storage.metrics_for(self.metrics.mode, app_category));
        if completion_persistence_warning(history_save_status, metric_failed).is_some() {
            notify(CompletionNotice::PersistenceFailed {
                history: history_save_status,
                metric_failed,
            });
        }
        if self.artifact_cleanup.cleanup().is_err() {
            notify(CompletionNotice::TempCleanupFailed);
        }
        // The take's record is queued after this job, so it includes this.
        self.trace.span_since(TakeSpan::CompletionJob, job_started);
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
    fn a_speculative_hit_counts_as_text_correction_but_stays_out_of_its_timings() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        let metrics = storage.metrics_for(PipelineMode::Dictate, None);
        record_text_correction_success(metrics, "openai", 900, false);
        record_text_correction_success(metrics, "openai", 40, true);
        let report = storage.diagnostics().unwrap();
        let row = report
            .summaries
            .iter()
            .find(|row| row.event_type == "text_correction")
            .unwrap();
        assert_eq!((row.count, row.successes, row.timed_count), (2, 2, 1));
        assert_eq!(row.p50_ms, Some(900));
    }

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

    const CAPTURE: &[u8] = b"RIFF deferred capture";

    /// A temporary capture owned for cleanup, as a recorded take has.
    fn temp_capture() -> (std::path::PathBuf, crate::TempArtifact) {
        static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "local-ai-voice-completion-test-{}-{}.wav",
            std::process::id(),
            NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, CAPTURE).unwrap();
        (path.clone(), crate::TempArtifact::new(path, false))
    }

    /// History on, keeping recordings.
    fn keeping_audio(storage: &crate::storage::Storage) {
        storage
            .update_settings(&types::Settings {
                delete_audio_after_processing: false,
                ..types::Settings::default()
            })
            .unwrap();
    }

    /// The job of an AI-corrected Dictate take that proposes a spelling.
    fn corrected_take_job(
        storage: &crate::storage::Storage,
        audio_path: std::path::PathBuf,
        artifact_cleanup: crate::TempArtifact,
    ) -> CompletionJob {
        CompletionJob {
            storage: storage.clone(),
            app_context: None,
            history: DeferredHistory {
                transcript_text: "open ai".into(),
                processed_text: Some("OpenAI".into()),
                mode: "ai_corrected",
                asr_provider: "mock".into(),
                llm_provider: Some("openai".into()),
                target_language: None,
                duration_ms: 1_000,
                latency_ms: 100,
                insertion_label: "clipboard_paste",
                insertion_detail: None,
                audio_path,
            },
            dictionary_candidate: Some(("open ai".into(), "OpenAI".into())),
            metrics: take_metrics(100),
            artifact_cleanup,
            trace: super::super::take_trace::TakeTrace::disabled(),
        }
    }

    type Notices = std::sync::Arc<std::sync::Mutex<Vec<CompletionNotice>>>;

    /// Queues `job` on `reservation` as a take does, recording its notices.
    fn queue(reservation: crate::storage::CompletionReservation, job: CompletionJob) -> Notices {
        let notices = Notices::default();
        let sink = notices.clone();
        reservation.defer(move || job.run(&mut |notice| sink.lock().unwrap().push(notice)));
        notices
    }

    fn history_rows(storage: &crate::storage::Storage) -> Vec<types::HistoryItem> {
        storage.list_history(types::HistoryFilter::All, 10).unwrap()
    }

    #[test]
    fn the_job_saves_history_before_the_candidate_metrics_and_cleanup() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        keeping_audio(&storage);
        let (path, cleanup) = temp_capture();
        let notices = queue(
            storage.reserve_completion(),
            corrected_take_job(&storage, path.clone(), cleanup),
        );
        assert!(storage.drain_deferred_completions(Duration::from_secs(10)));
        assert_eq!(
            *notices.lock().unwrap(),
            [
                CompletionNotice::HistoryChanged,
                CompletionNotice::DictionaryChanged
            ]
        );
        // The retained recording was copied before the capture was removed.
        let rows = history_rows(&storage);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].mode, "ai_corrected");
        assert!(rows[0].has_audio);
        assert_eq!(
            storage.history_audio(rows[0].id).unwrap().as_deref(),
            Some(CAPTURE)
        );
        assert!(!path.exists());
        assert_eq!(storage.list_dictionary_candidates().unwrap().len(), 1);
        assert_eq!(storage.metric_durations_for_test("dictation"), vec![100]);
    }

    #[test]
    fn a_persistence_warning_follows_the_change_events_and_cleanup_still_runs() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        keeping_audio(&storage);
        let (path, cleanup) = temp_capture();
        // A capture that cannot be copied leaves History text without audio.
        let missing = path.with_extension("missing.wav");
        let notices = queue(
            storage.reserve_completion(),
            corrected_take_job(&storage, missing, cleanup),
        );
        assert!(storage.drain_deferred_completions(Duration::from_secs(10)));
        assert_eq!(
            *notices.lock().unwrap(),
            [
                CompletionNotice::HistoryChanged,
                CompletionNotice::DictionaryChanged,
                CompletionNotice::PersistenceFailed {
                    history: crate::pipeline::finish::HistorySaveStatus::AudioUnavailable,
                    metric_failed: false,
                },
            ]
        );
        assert!(!history_rows(&storage)[0].has_audio);
        assert!(!path.exists());
    }

    /// Starts `deletion` after a take reserved its completion and checks it
    /// waits for the job, then removes what the job wrote.
    fn deletion_after_delivery(deletion: fn(&crate::storage::Storage)) {
        let storage = crate::storage::Storage::in_memory().unwrap();
        keeping_audio(&storage);
        let (path, cleanup) = temp_capture();
        let reservation = storage.reserve_completion();
        let deleting = {
            let storage = storage.clone();
            std::thread::spawn(move || deletion(&storage))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !deleting.is_finished(),
            "the deletion overtook a delivered take's deferred History save"
        );
        let notices = queue(
            reservation,
            corrected_take_job(&storage, path.clone(), cleanup),
        );
        deleting.join().unwrap();
        // The job ran first and wrote; the deletion then removed it all.
        assert_eq!(
            notices.lock().unwrap().first(),
            Some(&CompletionNotice::HistoryChanged)
        );
        assert!(history_rows(&storage).is_empty());
        assert!(storage.history_audio_files_for_test().is_empty());
        assert!(storage.diagnostics().unwrap().summaries.is_empty());
        assert!(!path.exists());
        keeping_audio(&storage);
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());
    }

    #[test]
    fn delete_all_after_delivery_removes_the_deferred_history_row_and_its_audio() {
        deletion_after_delivery(|storage| {
            assert_eq!(storage.delete_all_history().unwrap(), 1);
        });
    }

    #[test]
    fn turning_history_off_after_delivery_removes_the_deferred_history_row_and_its_audio() {
        deletion_after_delivery(|storage| {
            storage
                .update_settings_and_apply_history_policy(&types::Settings {
                    history_retention: types::HistoryRetention::Never,
                    delete_audio_after_processing: false,
                    ..types::Settings::default()
                })
                .unwrap();
        });
    }

    #[test]
    fn a_save_queued_after_history_was_turned_off_writes_nothing() {
        let storage = crate::storage::Storage::in_memory().unwrap();
        storage
            .update_settings_and_apply_history_policy(&types::Settings {
                history_retention: types::HistoryRetention::Never,
                delete_audio_after_processing: false,
                ..types::Settings::default()
            })
            .unwrap();
        let (path, cleanup) = temp_capture();
        let notices = queue(
            storage.reserve_completion(),
            corrected_take_job(&storage, path.clone(), cleanup),
        );
        assert!(storage.drain_deferred_completions(Duration::from_secs(10)));
        // Nothing changed, so the pages are not told to re-read.
        assert!(notices.lock().unwrap().is_empty());
        assert!(!path.exists());
        assert!(storage.history_audio_files_for_test().is_empty());
        assert!(storage.diagnostics().unwrap().summaries.is_empty());
        keeping_audio(&storage);
        assert!(history_rows(&storage).is_empty());
        assert!(storage.list_dictionary_candidates().unwrap().is_empty());
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

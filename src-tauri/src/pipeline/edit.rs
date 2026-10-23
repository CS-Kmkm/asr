//! Spoken Edit processing after recognition.
use super::{PipelineContext, RecordedTake};
use crate::commands::{
    ai_command_error, commit_edit_replacement, completion_persistence_warning,
    edit_failure_message, edit_persistence_warning, emit_correction_preview,
    persist_edit_completion, report_temp_cleanup, CommittedOutput, HistorySaveStatus,
    EDIT_CLIPBOARD_UNAVAILABLE,
};
use crate::{
    correction,
    correction::FailureOutcome,
    emit_stage, emit_state, emit_status, injection,
    injection::{InsertResult, InsertionDetail},
    recording_overlay,
    types::{self, AppPhase, NewHistoryItem, RecordingResult},
};
use std::time::Instant;
use tauri::Emitter;

pub(crate) async fn finish(
    context: PipelineContext<'_>,
    take: RecordedTake,
    mut edit_session: Option<crate::EditSession>,
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
        ..
    } = take;
    let duration_ms = artifact.duration.as_millis() as u64;
    let session = edit_session.take().ok_or("edit session is unavailable")?;
    let instruction_text = transcript.text.clone();
    let source_text = session.selection.text().to_owned();
    if instruction_text.trim().is_empty() {
        emit_state(
            app,
            state,
            AppPhase::Error,
            "No edit instruction was captured; the original selection was not changed.",
        );
        return Err("no edit instruction was captured".into());
    }
    emit_correction_preview(app, &instruction_text, "draft");
    emit_stage(
        app,
        state,
        types::AppStage::Correcting,
        "Applying the spoken edit instruction.",
    );
    let edit_started = Instant::now();
    let edited = match correction::edit_selected_text(
        &settings,
        &source_text,
        &instruction_text,
        correction_cancel,
        |delta| emit_correction_preview(app, delta, "streaming"),
    )
    .await
    {
        Ok(edited) => edited,
        Err(correction::CorrectionError::Cancelled) => {
            emit_state(app, state, AppPhase::Idle, "Editing cancelled.");
            return Err("editing was cancelled".into());
        }
        Err(error) => {
            let _ = storage.add_metric(
                "speak_to_edit",
                Some(settings.correction_provider.as_str()),
                Some(edit_started.elapsed().as_millis() as i64),
                false,
                Some("edit_failed"),
            );
            emit_state(app, state, AppPhase::Error, &edit_failure_message(&error));
            return Err(ai_command_error(
                app,
                &error,
                FailureOutcome::UnchangedSelection,
            ));
        }
    };
    emit_correction_preview(app, &edited, "final");
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(app, state, AppPhase::Idle, "Editing cancelled.");
        return Err("editing was cancelled".into());
    }
    session.monitor.wait_for_shortcut_release().await;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(app, state, AppPhase::Idle, "Editing cancelled.");
        return Err("editing was cancelled".into());
    }
    emit_state(
        app,
        state,
        AppPhase::Injecting,
        "Replacing the original selection.",
    );
    let (insertion, failure_detail) = match commit_edit_replacement(
        &services.lifecycle,
        operation_id,
        || {
            session.injector.replace_selection(
                &session.selection,
                &edited,
                &session.monitor,
                session.checkpoint,
            )
        },
        || session.injector.copy_to_clipboard(&edited),
    ) {
        CommittedOutput::Ran(outcome) => outcome,
        // Nothing was pasted or copied.
        CommittedOutput::Cancelled => {
            emit_state(app, state, AppPhase::Idle, "Editing cancelled.");
            return Err("editing was cancelled".into());
        }
        // Nothing was pasted or copied; the phase guard reports the
        // incomplete edit.
        CommittedOutput::Refused(error) => return Err(error.into()),
    };
    let clipboard_lost = session.injector.take_clipboard_not_restored();
    let restore_detail =
        insertion.and_then(|result| InsertionDetail::for_confirmed_paste(result, clipboard_lost));
    recording_overlay::set_phase(app, &AppPhase::Completed);
    let (insertion_label, completion) = match insertion {
        Some(InsertResult::ClipboardPaste) => ("clipboard_paste", "Selected text updated."),
        Some(InsertResult::ClipboardOnly) => (
            "clipboard_only",
            "Automatic replacement was skipped; the edit remains on the clipboard.",
        ),
        Some(InsertResult::PasteUnverified) => (
            "paste_unverified",
            "The edit paste could not be confirmed; the result remains on the clipboard.",
        ),
        None => ("insertion_failed", EDIT_CLIPBOARD_UNAVAILABLE),
    };
    let latency_ms = started.elapsed().as_millis() as u64;
    let (history_save_status, metric_failed) = persist_edit_completion(
        storage,
        &NewHistoryItem {
            transcript_text: &instruction_text,
            processed_text: Some(&edited),
            source_text: Some(&source_text),
            instruction_text: Some(&instruction_text),
            action_kind: None,
            search_site: None,
            mode: "edit",
            asr_provider: &transcript.model,
            llm_provider: Some(settings.correction_provider.as_str()),
            target_language: None,
            app_category: None,
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
            insertion_result: Some(insertion_label),
            insertion_detail: insertion
                .is_none()
                .then_some(failure_detail)
                .or(restore_detail)
                .map(InsertionDetail::code),
        },
        Some(&artifact.path),
        settings.correction_provider.as_str(),
        edit_started.elapsed().as_millis() as i64,
    );
    let Some(insertion) = insertion else {
        // Persist before returning: neither the target nor the clipboard
        // received the edit, and closing the app must not lose the result.
        state.publish_result(edited.clone());
        emit_state(app, state, AppPhase::Error, EDIT_CLIPBOARD_UNAVAILABLE);
        if let Some((kind, message)) =
            completion_persistence_warning(history_save_status, metric_failed)
        {
            emit_status(app, kind, message);
        }
        recording_overlay::show_insertion_notice(
            app,
            injection::notice_outcome(insertion_label, clipboard_lost),
            Some(failure_detail.code()),
        );
        report_temp_cleanup(app, &mut artifact_cleanup);
        return Err(EDIT_CLIPBOARD_UNAVAILABLE.into());
    };
    let snapshot = state.complete(edited.clone(), completion.into());
    let _ = app.emit("app-state", snapshot);
    emit_status(app, insertion_label, completion);
    if insertion != InsertResult::ClipboardPaste || clipboard_lost {
        recording_overlay::show_insertion_notice(
            app,
            injection::notice_outcome(insertion_label, clipboard_lost),
            None,
        );
    }
    let warning = if history_save_status == HistorySaveStatus::AudioUnavailable {
        completion_persistence_warning(history_save_status, metric_failed)
    } else {
        edit_persistence_warning(
            insertion,
            history_save_status == HistorySaveStatus::Failed,
            metric_failed,
        )
    };
    if let Some((kind, message)) = warning {
        emit_status(app, kind, message);
    }
    report_temp_cleanup(app, &mut artifact_cleanup);
    Ok(RecordingResult {
        text: edited,
        insertion: insertion_label.into(),
        duration_ms,
        latency_ms,
    })
}

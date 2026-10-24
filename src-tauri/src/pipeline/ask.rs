//! Ask planning, action policy and delivery.
use super::{PipelineContext, RecordedTake};
use crate::commands::{
    action_name, ai_command_error, ask_completion_message, ask_fallback_insertion, ask_insertion,
    cancellation_copy, commit_ask_copy, commit_ask_output, open_fixed_search,
    report_clipboard_not_restored, report_history_save_warning, report_temp_cleanup,
    require_ask_spoken_input, save_history, search_site_name,
};
use crate::{
    answer_panel, ask, command_error, correction,
    correction::FailureOutcome,
    emit_stage, emit_state, recording_overlay,
    types::{self, AppPhase, NewHistoryItem, RecordingResult},
    AskCapture, AskSession,
};
use std::sync::Arc;
use std::time::Instant;
use tauri::{Emitter, Manager};

pub(crate) async fn finish(
    context: PipelineContext<'_>,
    take: RecordedTake,
    mut ask_session: Option<AskSession>,
) -> Result<RecordingResult, String> {
    let PipelineContext {
        app,
        services,
        state,
        storage: _,
        operation_id,
    } = context;
    let RecordedTake {
        transcript,
        artifact,
        mut artifact_cleanup,
        started,
        mode,
        settings,
        app_context,
        ..
    } = take;
    let duration_ms = artifact.duration.as_millis() as u64;
    let (cancel_message, _) = cancellation_copy(mode);
    let Some(session) = ask_session.take() else {
        recording_overlay::set_phase(app, &AppPhase::Error);
        emit_state(app, state, AppPhase::Error, "Ask session is unavailable.");
        return Err("Ask session is unavailable".into());
    };
    let result = finish_ask(
        &context,
        Arc::new(session),
        &transcript.text,
        &transcript.model,
        duration_ms,
        started,
        Some(&artifact.path),
    )
    .await;
    if !services.lifecycle.is_cancelled(operation_id) {
        let metrics = context.storage.metrics_for(
            mode,
            crate::app_context::history_category(app_context.as_ref()),
        );
        let elapsed = result.as_ref().map_or_else(
            |_| started.elapsed().as_millis() as i64,
            |result| result.latency_ms as i64,
        );
        let _ = metrics.add_metric(
            "ask",
            Some(&settings.asr_backend),
            Some(elapsed),
            result.is_ok(),
            result.is_err().then_some("ask_failed"),
        );
        if let Ok(result) = &result {
            let inserted = matches!(result.insertion.as_str(), "selection" | "caret");
            let _ = metrics.add_metric(
                if matches!(result.insertion.as_str(), "panel" | "search") {
                    "ask_delivery"
                } else {
                    "insertion"
                },
                Some("ask"),
                Some(elapsed),
                inserted || matches!(result.insertion.as_str(), "panel" | "search"),
                (!inserted).then_some(result.insertion.as_str()),
            );
        }
    }
    if result.is_ok() {
        recording_overlay::set_phase(app, &AppPhase::Completed);
    }
    if let Err(error) = &result {
        recording_overlay::set_interactive(app, false);
        if services.lifecycle.is_cancelled(operation_id) {
            recording_overlay::set_phase(app, &AppPhase::Idle);
            emit_state(app, state, AppPhase::Idle, cancel_message);
        } else {
            recording_overlay::set_phase(app, &AppPhase::Error);
            emit_state(app, state, AppPhase::Error, error);
        }
    }
    report_temp_cleanup(app, &mut artifact_cleanup);
    result
}

async fn finish_ask(
    context: &PipelineContext<'_>,
    session: Arc<AskSession>,
    spoken: &str,
    asr_provider: &str,
    duration_ms: u64,
    started: Instant,
    audio_path: Option<&std::path::Path>,
) -> Result<RecordingResult, String> {
    let PipelineContext {
        app,
        services,
        state,
        storage,
        operation_id,
    } = *context;
    use ask::{AskAction, AskError};
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    require_ask_spoken_input(spoken)?;
    let settings = storage.get_settings().map_err(command_error)?;
    let app_context = services
        .app_context
        .lock()
        .map_err(|_| "target service is unavailable".to_owned())?
        .clone();
    let metrics = storage.metrics_for(
        crate::state::PipelineMode::Ask,
        crate::app_context::history_category(app_context.as_ref()),
    );
    emit_stage(
        app,
        state,
        types::AppStage::Planning,
        "Choosing a safe Ask action.",
    );
    // Planning receives spoken text and context only. It never receives source.
    let planning_personalization = super::personalization::resolve(
        storage,
        &settings,
        app_context.as_ref(),
        spoken,
        None,
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await
    .ok_or("ask was cancelled")?;
    let plan_started = Instant::now();
    let plan = correction::generate_ask_plan(
        &settings,
        spoken,
        &planning_personalization.instruction(&ask::planning_prompt(session.kind())),
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await;
    if !matches!(plan, Err(correction::CorrectionError::Cancelled)) {
        let _ = metrics.add_metric(
            "ask_plan",
            Some(&settings.correction_provider),
            Some(plan_started.elapsed().as_millis() as i64),
            plan.is_ok(),
            plan.as_ref().err().map(|error| error.failure_code()),
        );
    }
    let plan = plan.map_err(|error| ai_command_error(app, &error, FailureOutcome::NoAction))?;
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    let parsed =
        ask::parse_plan(&plan).map_err(|_| "Ask planner returned an invalid action".to_string())?;
    let action = match ask::validate_action(
        parsed,
        session.kind(),
        spoken,
        &settings.translation_target_languages,
    ) {
        Ok(action) => action,
        Err(AskError::Clarification(_)) => {
            let clarification =
                "Please name the target language, for example: translate to English.";
            if services.lifecycle.is_cancelled(operation_id) {
                return Err("ask was cancelled".into());
            }
            let published = services
                .answer_panel
                .publish_notice(operation_id, clarification.into());
            if !published || !answer_panel::show_notice(app, operation_id, clarification) {
                commit_ask_copy(&services.lifecycle, operation_id, || {
                    session.injector.copy_to_clipboard(clarification)
                })?;
                let snapshot = state.complete(
                    clarification.into(),
                    "Ask needs clarification; the prompt is on the clipboard.".into(),
                );
                let _ = app.emit("app-state", snapshot);
                return Ok(RecordingResult {
                    text: clarification.into(),
                    insertion: "clipboard_only".into(),
                    duration_ms,
                    latency_ms: started.elapsed().as_millis() as u64,
                });
            }
            let snapshot = state.complete(clarification.into(), "Ask needs clarification.".into());
            let _ = app.emit("app-state", snapshot);
            return Ok(RecordingResult {
                text: clarification.into(),
                insertion: "panel".into(),
                duration_ms,
                latency_ms: started.elapsed().as_millis() as u64,
            });
        }
        Err(AskError::Policy) => {
            return Err("Ask action was blocked by context policy".into());
        }
        Err(_) => return Err("Ask planner returned an invalid action".into()),
    };
    if let AskAction::Search { site } = &action {
        // The planner may choose a fixed site, but never the external payload.
        let query = site.derive_search_query(spoken);
        let url = site
            .fixed_url(spoken)
            .map_err(|_| "Ask search query was invalid".to_string())?;
        // Browser launch is irreversible. Cancellation either wins before
        // launch or observes a committed search; History and completion remain
        // owned by this operation until the outer PipelineGuard finishes it.
        services
            .lifecycle
            .commit_side_effect(operation_id, || open_fixed_search(&url))
            .map_err(|_| "ask was cancelled".to_string())??;
        let latency_ms = started.elapsed().as_millis() as u64;
        let history_save_status = save_history(
            storage,
            &NewHistoryItem {
                transcript_text: spoken,
                processed_text: Some(&query),
                source_text: session.selected_source(),
                instruction_text: Some(spoken),
                mode: "ask",
                asr_provider,
                llm_provider: Some(settings.correction_provider.as_str()),
                target_language: None,
                action_kind: Some("search"),
                search_site: Some(search_site_name(*site)),
                app_category: crate::app_context::history_category(app_context.as_ref()),
                duration_ms: Some(duration_ms as i64),
                latency_ms: Some(latency_ms as i64),
                retry_of_id: None,
                insertion_result: None,
                insertion_detail: None,
            },
            audio_path,
        );
        let snapshot = state.complete(query.clone(), "Opening the requested fixed search.".into());
        let _ = app.emit("app-state", snapshot);
        report_history_save_warning(app, history_save_status);
        return Ok(RecordingResult {
            text: query,
            insertion: "search".into(),
            duration_ms,
            latency_ms,
        });
    }
    let input = ask::generation_input(session.selected_source(), spoken);
    let generation_personalization = super::personalization::resolve(
        storage,
        &settings,
        app_context.as_ref(),
        spoken,
        session.selected_source(),
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await
    .ok_or("ask was cancelled")?;
    let generation_started = Instant::now();
    let output = correction::generate_ask_text(
        &settings,
        &input,
        &generation_personalization.instruction(&ask::generation_prompt_with_target(&action)),
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await;
    if !matches!(output, Err(correction::CorrectionError::Cancelled)) {
        let _ = metrics.add_metric(
            "ask_generation",
            Some(&settings.correction_provider),
            Some(generation_started.elapsed().as_millis() as i64),
            output.is_ok(),
            output.as_ref().err().map(|error| error.failure_code()),
        );
    }
    let output = output.map_err(|error| ai_command_error(app, &error, FailureOutcome::NoAction))?;
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    // Every paste or clipboard copy below goes through `commit_ask_output`.
    let app_for_insertion = app.clone();
    let session_for_insertion = Arc::clone(&session);
    let output_for_insertion = output.clone();
    let action_for_insertion = action.clone();
    let insertion = tokio::task::spawn_blocking(move || {
        let app = &app_for_insertion;
        let services = app.state::<crate::Services>();
        let session = session_for_insertion;
        let output = output_for_insertion;
        let action = action_for_insertion;
        let insertion = match (&action, &session.capture) {
            (
                AskAction::Rewrite
                | AskAction::Shorten
                | AskAction::Expand
                | AskAction::ChangeTone
                | AskAction::Translate { .. },
                AskCapture::Selected(selection),
            ) => {
                let cancel = services
                    .lifecycle
                    .cancellation(operation_id)
                    .map_err(command_error)?;
                commit_ask_output(&services.lifecycle, operation_id, || {
                    let replaced = session.monitor.as_ref().zip(session.checkpoint).map(
                        |(monitor, checkpoint)| {
                            session.injector.replace_selection_monitored(
                                selection, &output, monitor, checkpoint, &cancel,
                            )
                        },
                    );
                    ask_insertion(replaced, "selection", || {
                        session.injector.copy_to_clipboard(&output)
                    })
                })?
            }
            (AskAction::Draft, AskCapture::Caret(target)) => {
                let cancel = services
                    .lifecycle
                    .cancellation(operation_id)
                    .map_err(command_error)?;
                commit_ask_output(&services.lifecycle, operation_id, || {
                    let inserted = session
                        .monitor
                        .as_ref()
                        .zip(session.checkpoint)
                        .filter(|(monitor, checkpoint)| monitor.unchanged_since(*checkpoint))
                        .map(|(monitor, checkpoint)| {
                            session
                                .injector
                                .insert_monitored(&output, target, monitor, checkpoint, &cancel)
                        });
                    ask_insertion(inserted, "caret", || {
                        session.injector.copy_to_clipboard(&output)
                    })
                })?
            }
            _ => {
                if services.lifecycle.is_cancelled(operation_id) {
                    return Err("ask was cancelled".into());
                }
                let published = services.answer_panel.publish(operation_id, output.clone());
                if !published || !answer_panel::show(app, operation_id, &output) {
                    commit_ask_output(&services.lifecycle, operation_id, || {
                        Ok(ask_fallback_insertion(
                            session.injector.copy_to_clipboard(&output),
                        ))
                    })?
                } else {
                    "panel"
                }
            }
        };
        Ok::<_, String>(insertion)
    })
    .await
    .map_err(|_| "Ask insertion task failed".to_owned())??;
    // A committed paste or copy ignores later cancels, so only an answer
    // shown in the panel can still be cancelled here.
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    let latency_ms = started.elapsed().as_millis() as u64;
    let history_save_status = save_history(
        storage,
        &NewHistoryItem {
            transcript_text: spoken,
            processed_text: Some(&output),
            source_text: session.selected_source(),
            instruction_text: Some(spoken),
            mode: "ask",
            asr_provider,
            llm_provider: Some(settings.correction_provider.as_str()),
            target_language: match &action {
                AskAction::Translate { target_language } => Some(target_language.as_str()),
                _ => None,
            },
            action_kind: Some(action_name(&action)),
            search_site: None,
            app_category: crate::app_context::history_category(app_context.as_ref()),
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
            insertion_result: None,
            insertion_detail: None,
        },
        audio_path,
    );
    let message = ask_completion_message(insertion);
    let snapshot = state.complete(output.clone(), message.into());
    let _ = app.emit("app-state", snapshot);
    if session.injector.take_clipboard_not_restored() {
        report_clipboard_not_restored(app);
    }
    report_history_save_warning(app, history_save_status);
    Ok(RecordingResult {
        text: output,
        insertion: insertion.into(),
        duration_ms,
        latency_ms,
    })
}

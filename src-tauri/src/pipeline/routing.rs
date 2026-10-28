//! Automatic decisions run only after recognition, never during recording startup.
use super::{PipelineContext, RecordedTake};
use crate::{
    decision,
    sessions::{AskSession, EditSession},
    state::PipelineMode,
    types::AppStage,
};
use tauri::Emitter;

pub(super) async fn route(
    context: &PipelineContext<'_>,
    take: &mut RecordedTake,
    edit: &mut Option<EditSession>,
    ask: &mut Option<AskSession>,
) -> Result<(), String> {
    let Some(session) = ask.as_mut().filter(|session| session.automatic.is_some()) else {
        return Ok(());
    };
    if take.mode != PipelineMode::Dictate {
        return Err("automatic routing requires a Dictate recording".into());
    }
    let config = session.automatic.as_ref().expect("checked");
    let selected = session.selected_source().is_some();
    let _ = context.app.emit(
        "app-state",
        context.state.publish_result(take.transcript.text.clone()),
    );
    crate::emit_stage(
        context.app,
        context.state,
        AppStage::Planning,
        "Choosing a voice mode from your instruction.",
    );
    let decision = decision::classify(
        context.services.transcriber.as_ref(),
        config,
        &take.transcript.text,
        selected,
        take.correction_cancel.clone(),
    )
    .await;
    if context
        .services
        .lifecycle
        .is_cancelled(context.operation_id)
    {
        return Err("automatic voice routing was cancelled".into());
    }
    let detected = match decision {
        Ok(mode) => mode,
        Err(_) if decision::permits_dictation_fallback(session.kind()) => {
            crate::emit_status(
                context.app,
                "voice_mode_fallback",
                "Voice mode detection was unavailable or uncertain; using Dictate.",
            );
            PipelineMode::Dictate
        }
        Err(error) => {
            let message = decision::decision_failure_message(error);
            crate::emit_state(
                context.app,
                context.state,
                crate::types::AppPhase::Error,
                message,
            );
            return Err(message.into());
        }
    };
    // Classification cannot grant permission for a newly selected cloud operation.
    if let Err(error) = crate::privacy::check_recording(&take.settings, detected) {
        crate::emit_state(context.app, context.state, crate::types::AppPhase::Error, "Allow the required providers in Privacy before using this voice mode. The transcript remains available in this app.");
        return Err(error.into());
    }
    let settings = take.settings.clone();
    tokio::task::spawn_blocking(move || {
        crate::correction::preflight_recording(&settings, detected)
    })
    .await
    .map_err(|_| "voice mode preflight failed".to_string())?
    .map_err(|error| {
        let message = crate::commands::ai_command_error(
            context.app,
            &error,
            crate::correction::FailureOutcome::NoAction,
        );
        crate::emit_state(
            context.app,
            context.state,
            crate::types::AppPhase::Error,
            &message,
        );
        message
    })?;
    if context
        .services
        .lifecycle
        .is_cancelled(context.operation_id)
    {
        return Err("automatic voice routing was cancelled".into());
    }
    take.automatic = true;
    take.mode = detected;
    take.translation_target = None;
    match detected {
        PipelineMode::Edit => *edit = Some(session.take_edit()?),
        PipelineMode::Translate => {
            take.translation_target = Some(
                take.settings
                    .translation_target_languages
                    .iter()
                    .find(|language| {
                        crate::ask::translation_language_named_in(language, &take.transcript.text)
                    })
                    .cloned()
                    .unwrap_or_else(|| take.settings.translation_target_language.clone()),
            );
            if selected {
                take.mode = PipelineMode::Ask;
            }
        }
        _ => {}
    }
    let _ = context.app.emit("voice-mode", serde_json::json!({
        "mode": match detected { PipelineMode::Dictate => "dictate", PipelineMode::Translate => "translate", PipelineMode::Edit => "edit", PipelineMode::Ask => "ask", PipelineMode::SelectedText => unreachable!() },
        "targetLanguage": take.translation_target,
    }));
    if take.mode != PipelineMode::Ask {
        ask.take();
    }
    Ok(())
}

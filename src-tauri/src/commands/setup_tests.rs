//! Main-only setup tests reuse recording, cancellation and insertion guards.
use super::{
    begin_setup_insertion, cancel_pipeline_operation_for, start_claimed_recording,
    stop_recording_for, StopReason,
};
use crate::{
    emit_state, recording_overlay,
    state::{AppState, PipelineMode},
    storage::Storage,
    types::{AppPhase, RecordingResult},
    Services,
};
use tauri::{AppHandle, Manager};

#[tauri::command]
pub(crate) async fn start_dictation_test(app: AppHandle) -> Result<u64, String> {
    let id = super::MODEL_CHANGE_GATE
        .begin_pipeline(|| app.state::<Services>().lifecycle.begin_dictation_test())
        .map_err(str::to_owned)?;
    recording_overlay::anchor_to_foreground();
    tauri::async_runtime::spawn(async move {
        let _ = start_claimed_recording(
            app.clone(),
            app.state::<Services>(),
            app.state::<AppState>(),
            app.state::<Storage>(),
            false,
            None,
            PipelineMode::Dictate,
            id,
        )
        .await;
    });
    Ok(id)
}

#[tauri::command]
pub(crate) async fn stop_dictation_test(
    app: AppHandle,
    id: u64,
) -> Result<RecordingResult, String> {
    tauri::async_runtime::spawn(async move {
        let services = app.state::<Services>();
        if !services.lifecycle.is_dictation_test(id) {
            return Err("No dictation test is recording.".into());
        }
        stop_recording_for(
            app.clone(),
            services,
            app.state::<AppState>(),
            app.state::<Storage>(),
            StopReason::DictationTest(id),
        )
        .await
    })
    .await
    .map_err(|_| "Setup test failed.".to_owned())?
}

#[tauri::command]
pub(crate) async fn test_insertion(app: AppHandle) -> Result<u64, String> {
    let (id, cancel) = begin_setup_insertion(&app.state::<Services>())?;
    tauri::async_runtime::spawn(crate::pipeline::setup_tests::run_insertion(app, id, cancel));
    Ok(id)
}

#[tauri::command]
pub(crate) async fn cancel_setup_test(app: AppHandle, id: u64) -> Result<(), String> {
    tauri::async_runtime::spawn(async move {
        let services = app.state::<Services>();
        let state = app.state::<AppState>();
        if !services.lifecycle.is_setup_test(id) {
            return Ok(());
        }
        cancel_pipeline_operation_for(
            &services.lifecycle,
            &services.audio,
            &services.target,
            &services.voice_translation_target,
            &services.live,
            &services.edit,
            &services.ask,
            Some(id),
            || {
                recording_overlay::set_interactive(&app, false);
                emit_state(&app, &state, AppPhase::Idle, "Setup test cancelled.");
            },
        )
        .await?;
        Ok(())
    })
    .await
    .map_err(|_| "Setup test failed.".to_owned())?
}

use super::*;

fn save_history(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
) -> HistorySaveStatus {
    match storage.add_history_with_audio_report(item, audio_path) {
        Ok((_, true)) => HistorySaveStatus::AudioUnavailable,
        Ok((_, false)) => HistorySaveStatus::Complete,
        Err(_) => HistorySaveStatus::Failed,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HistorySaveStatus {
    Complete,
    AudioUnavailable,
    Failed,
}

fn report_history_save_warning(app: &AppHandle, status: HistorySaveStatus) {
    match status {
        HistorySaveStatus::Complete => {}
        HistorySaveStatus::AudioUnavailable => emit_status(
            app,
            "history_audio_unavailable",
            "History text was saved, but the recording could not be retained.",
        ),
        HistorySaveStatus::Failed => emit_status(
            app,
            "history_save_failed",
            "The result completed, but History could not be saved.",
        ),
    }
}

fn report_temp_cleanup(app: &AppHandle, artifact: &mut TempArtifact) {
    if artifact.cleanup().is_err() {
        emit_status(
            app,
            "artifact_cleanup_failed",
            "Temporary audio cleanup failed.",
        );
    }
}

fn capture_config(settings: &Settings) -> CaptureConfig {
    let noise_suppression = match settings.noise_suppression.as_str() {
        "off" => NoiseSuppressionLevel::Off,
        "low" => NoiseSuppressionLevel::Low,
        "high" => NoiseSuppressionLevel::High,
        _ => NoiseSuppressionLevel::Medium,
    };
    CaptureConfig {
        device_id: settings.microphone_id.clone(),
        enhancement: AudioEnhancementConfig {
            noise_suppression,
            gain: f32::from(settings.input_gain_percent) / 100.0,
            automatic_gain: settings.automatic_gain,
        },
        ..CaptureConfig::default()
    }
}

fn cancellation_copy(mode: PipelineMode) -> (&'static str, &'static str) {
    match mode {
        PipelineMode::Dictate => ("Dictation cancelled.", "dictation was cancelled"),
        PipelineMode::Translate => ("Translation cancelled.", "translation was cancelled"),
        PipelineMode::Edit => ("Editing cancelled.", "editing was cancelled"),
        PipelineMode::Ask => ("Ask cancelled.", "ask was cancelled"),
    }
}

fn persist_edit_completion(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
    provider: &str,
    elapsed_ms: i64,
) -> (HistorySaveStatus, bool) {
    // The target may already have accepted a paste. Persistence is best effort
    // so a database failure cannot turn that completed edit into command failure.
    let history_status = save_history(storage, item, audio_path);
    let metric_failed = storage
        .add_metric(
            "speak_to_edit",
            Some(provider),
            Some(elapsed_ms),
            true,
            None,
        )
        .is_err();
    (history_status, metric_failed)
}

async fn take_published_sessions<T, U>(
    live: &tokio::sync::Mutex<Option<T>>,
    edit: &tokio::sync::Mutex<Option<U>>,
    ask: &tokio::sync::Mutex<Option<AskSession>>,
    translation_target: &Mutex<Option<String>>,
) -> Result<(Option<T>, Option<U>, Option<AskSession>, Option<String>), String> {
    // Startup holds `live` while it publishes both the target language and the
    // session. Waiting for this lock prevents an immediate stop from observing
    // the lifecycle's Recording phase before that publication is complete.
    let live_task = live.lock().await.take();
    let edit_session = edit.lock().await.take();
    let ask_session = ask.lock().await.take();
    let target_language = translation_target
        .lock()
        .map_err(|_| "translation target service is unavailable".to_string())?
        .take();
    Ok((live_task, edit_session, ask_session, target_language))
}

fn validate_translation_targets(settings: &Settings) -> Result<(), String> {
    if settings.translation_target_languages.is_empty()
        || settings.translation_target_languages.len() > types::TRANSLATION_TARGET_LANGUAGES.len()
    {
        return Err("one to eight translation target languages are required".into());
    }
    for (index, language) in settings.translation_target_languages.iter().enumerate() {
        if !types::TRANSLATION_TARGET_LANGUAGES.contains(&language.as_str()) {
            return Err(format!(
                "unsupported translation target language: {language}"
            ));
        }
        if settings.translation_target_languages[..index].contains(language) {
            return Err("translation target languages must be unique".into());
        }
    }
    if !settings
        .translation_target_languages
        .contains(&settings.translation_target_language)
    {
        return Err("the current translation target must be in the configured list".into());
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn get_app_state(state: State<'_, AppState>) -> AppStateSnapshot {
    state.snapshot()
}

#[tauri::command]
pub(crate) async fn list_audio_devices(
    services: State<'_, Services>,
) -> Result<Vec<AudioDevice>, String> {
    let audio = services.audio.lock().await;
    audio.list_devices().await.map_err(command_error)
}

#[tauri::command]
pub(crate) async fn start_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_recording_with_origin(app, services, state, storage, false).await
}

#[tauri::command]
pub(crate) async fn start_voice_translation(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_voice_translation_with_origin(app, services, state, storage, false).await
}

#[tauri::command]
pub(crate) async fn start_speak_to_edit(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_speak_to_edit_with_origin(app, services, state, storage, false).await
}

#[tauri::command]
pub(crate) async fn start_ask(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_ask_with_origin(app, services, state, storage, false).await
}

pub(crate) async fn start_recording_with_origin(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        PipelineMode::Dictate,
    )
    .await
}

pub(crate) async fn start_voice_translation_with_origin(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        PipelineMode::Translate,
    )
    .await
}

pub(crate) async fn start_speak_to_edit_with_origin(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        PipelineMode::Edit,
    )
    .await
}

pub(crate) async fn start_ask_with_origin(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        PipelineMode::Ask,
    )
    .await
}

async fn start_recording_mode(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
    mode: PipelineMode,
) -> Result<(), String> {
    let (cancel_message, cancel_error) = cancellation_copy(mode);
    if services.translation_active.load(Ordering::Acquire) {
        return Err("selected-text translation is already active".into());
    }
    let operation_id = services.lifecycle.begin_start(mode)?;
    let guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    if services.translation_active.load(Ordering::Acquire) {
        return Err("selected-text translation is already active".into());
    }
    let settings = storage.get_settings().map_err(command_error)?;
    let mut edit_session = (mode == PipelineMode::Edit)
        .then(|| EditSession::new(&settings, from_shortcut))
        .transpose()?;
    let mut ask_session =
        (mode == PipelineMode::Ask).then(|| AskSession::new(&settings, from_shortcut));
    ensure_model_loaded(&app, &services, &settings).await?;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    let target = (mode != PipelineMode::Edit && mode != PipelineMode::Ask)
        .then(|| {
            SystemTextInjector::default()
                .capture_target()
                .map_err(command_error)
        })
        .transpose()?;
    let mut live_slot = services.live.lock().await;
    let mut edit_slot = services.edit.lock().await;
    let mut ask_slot = services.ask.lock().await;
    let active_hotkey = match mode {
        PipelineMode::Dictate => settings.hotkey.as_str(),
        PipelineMode::Translate => settings.voice_translate_hotkey.as_str(),
        PipelineMode::Edit => settings.speak_to_edit_hotkey.as_str(),
        PipelineMode::Ask => settings.ask_hotkey.as_str(),
    };
    let draft = target.as_ref().map(|target| {
        live_dictation::LiveDraft::new(
            target.clone(),
            &settings,
            active_hotkey,
            from_shortcut,
            mode == PipelineMode::Translate || mode == PipelineMode::Ask,
        )
    });
    let cancel = services.lifecycle.cancellation(operation_id)?;
    if let Some(session) = edit_session.as_ref() {
        session.monitor.observe_cancellation(Some(cancel.clone()));
    }
    if let Some(session) = ask_session.as_ref() {
        if let Some(monitor) = &session.monitor {
            monitor.observe_cancellation(Some(cancel.clone()));
        }
    }
    let dictionary_terms = storage.dictionary_prompt_terms().unwrap_or_default();
    let prompt = (!dictionary_terms.is_empty()).then(|| dictionary_terms.join("\n"));
    let capture_config = capture_config(&settings);
    let mut audio = services.audio.lock().await;
    // Open the stream only when dictation starts. Keeping a Bluetooth headset
    // microphone armed while idle forces Windows to retain the low-fidelity
    // hands-free profile for playback.
    async {
        audio.arm(capture_config.clone()).await?;
        audio.start(capture_config).await
    }
    .await
    .map_err(command_error)?;
    if let Err(error) = services.lifecycle.mark_recording(operation_id) {
        audio.cancel().await.map_err(command_error)?;
        return Err(error.into());
    }
    drop(audio);
    *services
        .target
        .lock()
        .map_err(|_| "target service is unavailable".to_string())? = target;
    let target_language =
        (mode == PipelineMode::Translate).then(|| settings.translation_target_language.clone());
    *services
        .voice_translation_target
        .lock()
        .map_err(|_| "translation target service is unavailable".to_string())? =
        target_language.clone();
    let _ = app.emit(
        "voice-mode",
        serde_json::json!({
            "mode": match mode {
                PipelineMode::Dictate => "dictate",
                PipelineMode::Translate => "translate",
                PipelineMode::Edit => "edit",
                PipelineMode::Ask => "ask",
            },
            "targetLanguage": target_language,
        }),
    );
    state.clear_result();
    emit_state(
        &app,
        &state,
        AppPhase::Recording,
        match mode {
            PipelineMode::Translate => "Recording speech to translate.",
            PipelineMode::Edit => "Recording an edit instruction.",
            PipelineMode::Ask => "Recording an Ask instruction.",
            PipelineMode::Dictate => "Recording from the selected microphone.",
        },
    );
    recording_overlay::set_interactive(&app, mode == PipelineMode::Translate);
    *live_slot =
        draft.map(|draft| live_dictation::start(app.clone(), operation_id, draft, prompt, cancel));
    *edit_slot = edit_session.take();
    *ask_slot = ask_session.take();
    drop(ask_slot);
    drop(edit_slot);
    drop(live_slot);
    std::mem::forget(guard);

    let app_for_levels = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let Some(services) = app_for_levels.try_state::<Services>() else {
                break;
            };
            if services.lifecycle.phase() != PipelinePhase::Recording
                || services.lifecycle.is_cancelled(operation_id)
            {
                break;
            }
            let level = {
                let audio = services.audio.lock().await;
                audio.level()
            };
            let _ = app_for_levels.emit("audio-level", level);
        }
    });
    Ok(())
}

#[tauri::command]
pub(crate) async fn stop_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<RecordingResult, String> {
    let (operation_id, mode, cancel) = match services.lifecycle.begin_processing() {
        Ok(operation) => operation,
        Err(error) => {
            if error == "no recording is active" {
                emit_state(
                    &app,
                    &state,
                    AppPhase::Idle,
                    "No recording is active. Start a new recording.",
                );
            }
            return Err(error.into());
        }
    };
    let _guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    let (cancel_message, cancel_error) = cancellation_copy(mode);
    recording_overlay::set_interactive(&app, false);
    // Publish the lifecycle transition immediately. Previously the UI stayed
    // in Recording until audio finalization completed; if finalization failed
    // (for example for a short or silent take), the lifecycle returned to Idle
    // while the UI incorrectly continued to offer "Stop and transcribe".
    emit_state(
        &app,
        &state,
        AppPhase::Processing,
        "Stopping recording and preparing audio.",
    );
    let started = Instant::now();
    let (live_task, mut edit_session, mut ask_session, translation_target) =
        take_published_sessions(
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
    // Keep the full artifact owned even if joining live work or loading settings fails.
    let mut artifact_cleanup = artifact_result
        .as_ref()
        .ok()
        .map(|artifact| TempArtifact::new(artifact.path.clone(), false));
    let mut draft = match live_task {
        Some(task) => Some(task.finish().await?),
        None if mode == PipelineMode::Edit || mode == PipelineMode::Ask => None,
        None => return Err("live dictation session is unavailable".into()),
    };
    let settings = storage.get_settings().map_err(command_error)?;
    let artifact = artifact_result.map_err(|error| {
        emit_state(
            &app,
            &state,
            AppPhase::Error,
            "No usable speech was captured. Start a new recording and try again.",
        );
        command_error(error)
    })?;
    let duration_ms = artifact.duration.as_millis() as u64;
    let mut artifact_cleanup = artifact_cleanup
        .take()
        .expect("successful audio has cleanup ownership");
    // The temporary capture remains cleanup-owned until any retained History
    // copy has committed. History never takes ownership of this path.
    artifact_cleanup.retain = false;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    emit_state(&app, &state, AppPhase::Processing, "Transcribing locally.");

    let dictionary_terms = storage.dictionary_prompt_terms().unwrap_or_default();
    let prompt = (!dictionary_terms.is_empty()).then(|| dictionary_terms.join("\n"));
    let correction_cancel = cancel.clone();
    let transcript_result = services
        .transcriber
        .transcribe(&artifact.path, prompt.as_deref(), cancel)
        .await;
    let transcript = match transcript_result {
        Ok(value) => value,
        Err(asr::AsrError::Cancelled) => {
            report_temp_cleanup(&app, &mut artifact_cleanup);
            emit_state(&app, &state, AppPhase::Idle, cancel_message);
            return Err(cancel_error.into());
        }
        Err(error) => {
            let _ = storage.add_metric(
                "asr",
                Some(settings.asr_backend.as_str()),
                None,
                false,
                Some("asr_failed"),
            );
            emit_state(
                &app,
                &state,
                AppPhase::Error,
                "Transcription failed. Check model and GPU diagnostics.",
            );
            report_temp_cleanup(&app, &mut artifact_cleanup);
            return Err(command_error(error));
        }
    };
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }

    services
        .target
        .lock()
        .map_err(|_| "target service is unavailable".to_string())?
        .take();
    if mode == PipelineMode::Edit {
        let session = edit_session.take().ok_or("edit session is unavailable")?;
        let instruction_text = transcript.text.clone();
        let source_text = session.selection.text().to_owned();
        if instruction_text.trim().is_empty() {
            emit_state(
                &app,
                &state,
                AppPhase::Error,
                "No edit instruction was captured; the original selection was not changed.",
            );
            return Err("no edit instruction was captured".into());
        }
        emit_correction_preview(&app, &instruction_text, "draft");
        emit_state(
            &app,
            &state,
            AppPhase::Processing,
            "Applying the spoken edit instruction.",
        );
        let edit_started = Instant::now();
        let edited = match correction::edit_selected_text(
            &settings,
            &source_text,
            &instruction_text,
            correction_cancel,
            |delta| emit_correction_preview(&app, delta, "streaming"),
        )
        .await
        {
            Ok(edited) => edited,
            Err(correction::CorrectionError::Cancelled) => {
                emit_state(&app, &state, AppPhase::Idle, "Editing cancelled.");
                return Err("editing was cancelled".into());
            }
            Err(_error) => {
                let _ = storage.add_metric(
                    "speak_to_edit",
                    Some(settings.correction_provider.as_str()),
                    Some(edit_started.elapsed().as_millis() as i64),
                    false,
                    Some("edit_failed"),
                );
                emit_state(
                    &app,
                    &state,
                    AppPhase::Error,
                    "Editing failed; the original selection was not changed.",
                );
                return Err("editing failed; the original selection was not changed".into());
            }
        };
        emit_correction_preview(&app, &edited, "final");
        if services.lifecycle.is_cancelled(operation_id) {
            emit_state(&app, &state, AppPhase::Idle, "Editing cancelled.");
            return Err("editing was cancelled".into());
        }
        session.monitor.wait_for_shortcut_release().await;
        if services.lifecycle.is_cancelled(operation_id) {
            emit_state(&app, &state, AppPhase::Idle, "Editing cancelled.");
            return Err("editing was cancelled".into());
        }
        emit_state(
            &app,
            &state,
            AppPhase::Injecting,
            "Replacing the original selection.",
        );
        let insertion = match session.injector.replace_selection(
            &session.selection,
            &edited,
            &session.monitor,
            session.checkpoint,
        ) {
            Ok(result) => result,
            Err(_) => {
                session
                    .injector
                    .copy_to_clipboard(&edited)
                    .map_err(command_error)?;
                InsertResult::ClipboardOnly
            }
        };
        recording_overlay::set_phase(&app, &AppPhase::Completed);
        let latency_ms = started.elapsed().as_millis() as u64;
        let (history_save_status, metric_failed) = persist_edit_completion(
            &storage,
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
            },
            Some(&artifact.path),
            settings.correction_provider.as_str(),
            edit_started.elapsed().as_millis() as i64,
        );
        let (insertion_label, completion) = match insertion {
            InsertResult::ClipboardPaste => ("clipboard_paste", "Selected text updated."),
            InsertResult::ClipboardOnly => (
                "clipboard_only",
                "Automatic replacement was skipped; the edit remains on the clipboard.",
            ),
            InsertResult::PasteUnverified => (
                "paste_unverified",
                "The edit paste could not be confirmed; the result remains on the clipboard.",
            ),
        };
        let snapshot = state.complete(edited.clone(), completion.into());
        let _ = app.emit("app-state", snapshot);
        emit_status(&app, insertion_label, completion);
        if let Some((kind, message)) =
            completion_persistence_warning(history_save_status, metric_failed)
        {
            emit_status(&app, kind, message);
        }
        report_temp_cleanup(&app, &mut artifact_cleanup);
        return Ok(RecordingResult {
            text: edited,
            insertion: insertion_label.into(),
            duration_ms,
            latency_ms,
        });
    }
    if mode == PipelineMode::Ask {
        let Some(session) = ask_session.take() else {
            recording_overlay::set_phase(&app, &AppPhase::Error);
            emit_state(&app, &state, AppPhase::Error, "Ask session is unavailable.");
            return Err("Ask session is unavailable".into());
        };
        let result = finish_ask(
            &app,
            &services,
            &state,
            &storage,
            operation_id,
            &session,
            &transcript.text,
            &transcript.model,
            duration_ms,
            started,
            Some(&artifact.path),
        )
        .await;
        if let Err(error) = &result {
            recording_overlay::set_interactive(&app, false);
            if services.lifecycle.is_cancelled(operation_id) {
                recording_overlay::set_phase(&app, &AppPhase::Idle);
                emit_state(&app, &state, AppPhase::Idle, cancel_message);
            } else {
                recording_overlay::set_phase(&app, &AppPhase::Error);
                emit_state(&app, &state, AppPhase::Error, error);
            }
        }
        report_temp_cleanup(&app, &mut artifact_cleanup);
        return result;
    }
    let mut draft = draft
        .take()
        .expect("non-edit recording has a live dictation session");
    let mut final_text = transcript.text.clone();
    let mut processed_text = None;
    let mut llm_provider = None;
    let mut correction_failed = false;
    let mut translation_failed = false;
    let _ = app.emit("app-state", state.publish_result(transcript.text.clone()));
    // Full-recording recognition reconciles the last live hypothesis before AI correction.
    draft.monitor.wait_for_shortcut_release().await;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    if let Err(error) = draft.update(&transcript.text) {
        emit_status(
            &app,
            "streaming_insertion_unavailable",
            &format!("Draft insertion failed; the transcript is available in this app. {error}"),
        );
    }
    let streamed_into_target = draft.pasted;
    if mode == PipelineMode::Translate {
        let target_language = translation_target
            .as_deref()
            .ok_or("translation target language is unavailable")?;
        emit_correction_preview(&app, &transcript.text, "draft");
        emit_state(
            &app,
            &state,
            AppPhase::Processing,
            "Translating speech to the selected language.",
        );
        let translation_started = Instant::now();
        llm_provider = Some(settings.correction_provider.clone());
        match correction::translate_transcript(
            &settings,
            &transcript.text,
            target_language,
            correction_cancel,
            |delta| emit_correction_preview(&app, delta, "streaming"),
        )
        .await
        {
            Ok(translated) => {
                final_text = translated;
                processed_text = Some(final_text.clone());
                emit_correction_preview(&app, &final_text, "final");
                let _ = storage.add_metric(
                    "voice_translation",
                    Some(settings.correction_provider.as_str()),
                    Some(translation_started.elapsed().as_millis() as i64),
                    true,
                    None,
                );
            }
            Err(correction::CorrectionError::Cancelled) => {
                emit_state(&app, &state, AppPhase::Idle, "Translation cancelled.");
                return Err("translation was cancelled".into());
            }
            Err(_) => {
                translation_failed = true;
                emit_correction_preview(&app, &transcript.text, "fallback");
                let _ = storage.add_metric(
                    "voice_translation",
                    Some(settings.correction_provider.as_str()),
                    Some(translation_started.elapsed().as_millis() as i64),
                    false,
                    Some("translation_failed"),
                );
                emit_status(
                    &app,
                    "voice_translation_failed",
                    "Translation failed; the raw transcript remains on the clipboard.",
                );
            }
        }
    } else if settings.text_correction_enabled {
        let correction_hints = storage
            .dictionary_correction_hints(&transcript.text)
            .unwrap_or_default();
        emit_correction_preview(&app, &transcript.text, "draft");
        emit_state(
            &app,
            &state,
            AppPhase::Injecting,
            "Inserting the provisional transcript into the captured target.",
        );
        emit_state(
            &app,
            &state,
            AppPhase::Processing,
            "Correcting the transcript with the configured AI provider.",
        );
        let correction_started = Instant::now();
        match correction::correct_transcript(
            &settings,
            &transcript.text,
            &correction_hints,
            correction_cancel,
            |delta| {
                emit_correction_preview(&app, delta, "streaming");
            },
        )
        .await
        {
            Ok(corrected) => {
                final_text = corrected;
                emit_correction_preview(&app, &final_text, "final");
                processed_text = Some(final_text.clone());
                llm_provider = Some(settings.correction_provider.clone());
                let _ = storage.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_started.elapsed().as_millis() as i64),
                    true,
                    None,
                );
            }
            Err(correction::CorrectionError::Cancelled) => {
                emit_state(&app, &state, AppPhase::Idle, "Dictation cancelled.");
                return Err("dictation was cancelled".into());
            }
            Err(error) => {
                correction_failed = true;
                emit_correction_preview(&app, &transcript.text, "fallback");
                let _ = storage.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_started.elapsed().as_millis() as i64),
                    false,
                    Some("correction_failed"),
                );
                emit_status(
                    &app,
                    "text_correction_failed",
                    &correction_failure_status(&error),
                );
            }
        }
    } else {
        emit_correction_preview(&app, &transcript.text, "final");
    }
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }

    emit_state(
        &app,
        &state,
        AppPhase::Injecting,
        if streamed_into_target {
            "Finalizing the provisional text in the captured target."
        } else {
            "Inserting into the captured target."
        },
    );
    let insertion_result = if translation_failed {
        draft
            .injector
            .copy_to_clipboard(&final_text)
            .map(|()| InsertResult::ClipboardOnly)
    } else {
        draft.finish(&final_text)
    };
    // Insertion has returned; helper shutdown and persistence are not insertion.
    // This only hides the overlay. The result below still determines success.
    recording_overlay::set_phase(&app, &AppPhase::Completed);
    // The helper observes input only while a provisional replacement session
    // can still mutate the target. Stop it before persisting the result.
    drop(draft);
    let insertion = insertion_result.map_err(|error| {
        emit_state(
            &app,
            &state,
            AppPhase::Error,
            "Insertion was blocked for safety.",
        );
        command_error(error)
    })?;
    let latency_ms = started.elapsed().as_millis() as u64;
    let insertion_label = if streamed_into_target && insertion == InsertResult::ClipboardPaste {
        "provisional_replace"
    } else {
        match insertion {
            InsertResult::ClipboardPaste => "clipboard_paste",
            InsertResult::ClipboardOnly => "clipboard_only",
            InsertResult::PasteUnverified => "paste_unverified",
        }
    };
    let history_save_status = save_history(
        &storage,
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
            app_category: None,
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
        },
        Some(&artifact.path),
    );
    let metric_result = storage.add_metric(
        if mode == PipelineMode::Translate {
            "voice_translate"
        } else {
            "dictation"
        },
        Some(&transcript.model),
        Some(latency_ms as i64),
        !translation_failed,
        translation_failed.then_some("translation_failed"),
    );
    let completion = if translation_failed {
        "Translation failed; the raw transcript remains on the clipboard."
    } else if mode == PipelineMode::Translate && insertion == InsertResult::PasteUnverified {
        "Translation paste could not be confirmed; the result remains available in this app."
    } else if mode == PipelineMode::Translate && insertion == InsertResult::ClipboardOnly {
        "The target changed; the translation remains on the clipboard."
    } else if mode == PipelineMode::Translate {
        "Voice translation inserted."
    } else if insertion == InsertResult::PasteUnverified {
        "Paste completion could not be confirmed. Check the input target before pasting again; the completed result is available in this app."
    } else if insertion == InsertResult::ClipboardOnly && streamed_into_target {
        "The provisional text could not be safely replaced; the final result remains on the clipboard."
    } else if insertion == InsertResult::ClipboardOnly {
        "Automatic insertion failed; the result remains on the clipboard."
    } else if correction_failed {
        "AI correction failed; the original transcript was inserted."
    } else {
        "Dictation inserted successfully."
    };
    let snapshot = state.complete(final_text.clone(), completion.into());
    let _ = app.emit("app-state", snapshot);
    emit_status(&app, insertion_label, completion);
    if let Some((kind, message)) =
        completion_persistence_warning(history_save_status, metric_result.is_err())
    {
        emit_status(&app, kind, message);
    }
    report_temp_cleanup(&app, &mut artifact_cleanup);
    Ok(RecordingResult {
        text: final_text,
        insertion: insertion_label.into(),
        duration_ms,
        latency_ms,
    })
}

#[allow(clippy::too_many_arguments)]
async fn finish_ask(
    app: &AppHandle,
    services: &Services,
    state: &AppState,
    storage: &Storage,
    operation_id: u64,
    session: &AskSession,
    spoken: &str,
    asr_provider: &str,
    duration_ms: u64,
    started: Instant,
    audio_path: Option<&std::path::Path>,
) -> Result<RecordingResult, String> {
    use ask::{AskAction, AskError};
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    let settings = storage.get_settings().map_err(command_error)?;
    emit_state(
        app,
        state,
        AppPhase::Processing,
        "Choosing a safe Ask action.",
    );
    // Planning receives spoken text and context only. It never receives source.
    let plan = correction::generate_ask_plan(
        &settings,
        spoken,
        &ask::planning_prompt(session.kind()),
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await
    .map_err(|_| "Ask planning failed without taking an action".to_string())?;
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
                .publish(operation_id, clarification.into());
            if !published || !answer_panel::show(app, operation_id, clarification) {
                session
                    .injector
                    .copy_to_clipboard(clarification)
                    .map_err(command_error)?;
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
        let query = spoken.trim();
        let url = site
            .fixed_url(query)
            .map_err(|_| "Ask search query was invalid".to_string())?;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("ask was cancelled".into());
        }
        open_fixed_search(&url)?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let history_save_status = save_history(
            storage,
            &NewHistoryItem {
                transcript_text: spoken,
                processed_text: Some(query),
                source_text: session.selected_source(),
                instruction_text: Some(spoken),
                mode: "ask",
                asr_provider,
                llm_provider: Some(settings.correction_provider.as_str()),
                target_language: None,
                action_kind: Some("search"),
                search_site: Some(search_site_name(*site)),
                app_category: None,
                duration_ms: Some(duration_ms as i64),
                latency_ms: Some(latency_ms as i64),
                retry_of_id: None,
            },
            audio_path,
        );
        let snapshot = state.complete(
            query.to_string(),
            "Opening the requested fixed search.".into(),
        );
        let _ = app.emit("app-state", snapshot);
        report_history_save_warning(app, history_save_status);
        return Ok(RecordingResult {
            text: query.into(),
            insertion: "search".into(),
            duration_ms,
            latency_ms,
        });
    }
    let input = ask::generation_input(session.selected_source(), spoken);
    let output = correction::generate_ask_text(
        &settings,
        &input,
        &ask::generation_prompt_with_target(&action),
        services
            .lifecycle
            .cancellation(operation_id)
            .map_err(command_error)?,
    )
    .await
    .map_err(|_| "Ask generation failed without taking an action".to_string())?;
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    let mut insertion = "panel";
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("ask was cancelled".into());
    }
    match (&action, &session.capture) {
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
            let result = session.monitor.as_ref().zip(session.checkpoint).and_then(
                |(monitor, checkpoint)| {
                    session
                        .injector
                        .replace_selection(selection, &output, monitor, checkpoint)
                        .ok()
                },
            );
            match result {
                Some(InsertResult::ClipboardPaste) => insertion = "selection",
                Some(InsertResult::ClipboardOnly) => {
                    session
                        .injector
                        .copy_to_clipboard(&output)
                        .map_err(command_error)?;
                    insertion = "clipboard_only";
                }
                Some(InsertResult::PasteUnverified) => insertion = "paste_unverified",
                None if !*cancel.borrow() => {
                    session
                        .injector
                        .copy_to_clipboard(&output)
                        .map_err(command_error)?;
                    insertion = "clipboard_only";
                }
                None => return Err("ask was cancelled".into()),
            }
        }
        (AskAction::Draft, AskCapture::Caret(target)) => {
            let cancel = services
                .lifecycle
                .cancellation(operation_id)
                .map_err(command_error)?;
            let result = session
                .monitor
                .as_ref()
                .zip(session.checkpoint)
                .filter(|(monitor, checkpoint)| monitor.unchanged_since(*checkpoint))
                .and_then(|(monitor, checkpoint)| {
                    session
                        .injector
                        .insert_monitored(&output, target, monitor, checkpoint, &cancel)
                        .ok()
                });
            match result {
                Some(InsertResult::ClipboardPaste) => insertion = "caret",
                Some(InsertResult::ClipboardOnly) => {
                    session
                        .injector
                        .copy_to_clipboard(&output)
                        .map_err(command_error)?;
                    insertion = "clipboard_only";
                }
                Some(InsertResult::PasteUnverified) => insertion = "paste_unverified",
                None if !*cancel.borrow() => {
                    session
                        .injector
                        .copy_to_clipboard(&output)
                        .map_err(command_error)?;
                    insertion = "clipboard_only";
                }
                None => return Err("ask was cancelled".into()),
            }
        }
        _ => {
            if services.lifecycle.is_cancelled(operation_id) {
                return Err("ask was cancelled".into());
            }
            let published = services.answer_panel.publish(operation_id, output.clone());
            if !published || !answer_panel::show(app, operation_id, &output) {
                let _ = session.injector.copy_to_clipboard(&output);
                insertion = "clipboard_only";
            }
        }
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
            app_category: None,
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
        },
        audio_path,
    );
    let message = if insertion == "paste_unverified" {
        "Ask insertion could not be confirmed; the result remains on the clipboard."
    } else if insertion == "clipboard_only" {
        "Ask result is on the clipboard."
    } else {
        "Ask completed."
    };
    let snapshot = state.complete(output.clone(), message.into());
    let _ = app.emit("app-state", snapshot);
    report_history_save_warning(app, history_save_status);
    Ok(RecordingResult {
        text: output,
        insertion: insertion.into(),
        duration_ms,
        latency_ms,
    })
}

fn action_name(action: &ask::AskAction) -> &'static str {
    match action {
        ask::AskAction::Rewrite => "rewrite",
        ask::AskAction::Shorten => "shorten",
        ask::AskAction::Expand => "expand",
        ask::AskAction::ChangeTone => "change_tone",
        ask::AskAction::Summarize => "summarize",
        ask::AskAction::Explain => "explain",
        ask::AskAction::Translate { .. } => "translate",
        ask::AskAction::Answer => "answer",
        ask::AskAction::Draft => "draft",
        ask::AskAction::Search { .. } => "search",
    }
}
fn search_site_name(site: ask::SearchSite) -> &'static str {
    match site {
        ask::SearchSite::Google => "google",
        ask::SearchSite::YouTube => "youtube",
        ask::SearchSite::AmazonJapan => "amazon_japan",
        ask::SearchSite::GitHub => "github",
    }
}

#[cfg(target_os = "windows")]
fn open_fixed_search(url: &str) -> Result<(), String> {
    use windows::{
        core::{HSTRING, PCWSTR},
        Win32::{
            Foundation::HWND,
            UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        },
    };
    let url = HSTRING::from(url);
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            None,
            PCWSTR(url.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        Err("The fixed search could not be opened.".into())
    } else {
        Ok(())
    }
}
#[cfg(not(target_os = "windows"))]
fn open_fixed_search(_: &str) -> Result<(), String> {
    Err("Search launch is only supported on Windows.".into())
}

fn persistence_warning(
    history_failed: bool,
    metric_failed: bool,
) -> Option<(&'static str, &'static str)> {
    match (history_failed, metric_failed) {
        (true, true) => Some((
            "history_and_metric_save_failed",
            "Text was published, but History and usage metrics could not be saved.",
        )),
        (true, false) => Some((
            "history_save_failed",
            "Text was published, but History could not be saved.",
        )),
        (false, true) => Some((
            "metric_save_failed",
            "Text was published, but usage metrics could not be saved.",
        )),
        (false, false) => None,
    }
}

fn completion_persistence_warning(
    history_status: HistorySaveStatus,
    metric_failed: bool,
) -> Option<(&'static str, &'static str)> {
    if history_status == HistorySaveStatus::AudioUnavailable {
        return if metric_failed {
            Some((
                "history_audio_and_metric_save_failed",
                "History text was saved, but the recording and usage metrics could not be retained.",
            ))
        } else {
            Some((
                "history_audio_unavailable",
                "History text was saved, but the recording could not be retained.",
            ))
        };
    }
    persistence_warning(history_status == HistorySaveStatus::Failed, metric_failed)
}

/// The single persistence step of `update_settings`. Settings and their
/// History retention (including the History-off purge) commit in one SQLite
/// transaction, so a failed settings write cannot erase History and an insert
/// cannot observe the old settings after the purge. Any failure runs
/// `rollback` so registered shortcuts match the unchanged stored settings.
fn persist_settings_or_rollback(
    storage: &Storage,
    settings: &Settings,
    rollback: impl FnOnce(),
) -> Result<(), String> {
    storage
        .update_settings_and_apply_history_policy(settings)
        .map_err(|error| {
            rollback();
            command_error(error)
        })
}

fn hotkey_changes(registered: &[Shortcut], desired: &[Shortcut]) -> (Vec<Shortcut>, Vec<Shortcut>) {
    (
        registered
            .iter()
            .copied()
            .filter(|shortcut| !desired.contains(shortcut))
            .collect(),
        desired
            .iter()
            .copied()
            .filter(|shortcut| !registered.contains(shortcut))
            .collect(),
    )
}

fn hotkeys_repaired(registered: &[Shortcut], desired: &[Shortcut]) -> bool {
    desired.len() == 5 && desired.iter().all(|shortcut| registered.contains(shortcut))
}

#[tauri::command]
pub(crate) fn get_startup_hotkey_warning(services: State<'_, Services>) -> Option<String> {
    services
        .startup_hotkey_issues
        .lock()
        .ok()?
        .message()
        .map(str::to_owned)
}

fn emit_correction_preview(app: &AppHandle, text: &str, stage: &str) {
    let _ = app.emit(
        "correction-preview",
        serde_json::json!({ "text": text, "stage": stage }),
    );
}

#[tauri::command]
pub(crate) fn cycle_voice_translation_target(
    app: AppHandle,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<String, String> {
    let mut active = services
        .voice_translation_target
        .lock()
        .map_err(|_| "translation target service is unavailable".to_string())?;
    if services.lifecycle.phase() != PipelinePhase::Recording
        || services.lifecycle.mode() != Some(PipelineMode::Translate)
    {
        return Err("voice translation is not recording".into());
    }
    let mut settings = storage.get_settings().map_err(command_error)?;
    if settings.translation_target_languages.is_empty() {
        return Err("at least one translation target language is required".into());
    }
    let current = active
        .as_deref()
        .unwrap_or(settings.translation_target_language.as_str());
    let current_index = settings
        .translation_target_languages
        .iter()
        .position(|language| language == current)
        .unwrap_or(0);
    let next = settings.translation_target_languages
        [(current_index + 1) % settings.translation_target_languages.len()]
    .clone();
    settings.translation_target_language = next.clone();
    storage.update_settings(&settings).map_err(command_error)?;
    *active = Some(next.clone());
    let _ = app.emit("settings-changed", settings);
    let _ = app.emit(
        "voice-mode",
        serde_json::json!({ "mode": "translate", "targetLanguage": next }),
    );
    Ok(next)
}

#[tauri::command]
pub(crate) async fn cancel_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let mode = services.lifecycle.mode().unwrap_or(PipelineMode::Dictate);
    let cancelled = cancel_pipeline_operation(
        &services.lifecycle,
        &services.audio,
        &services.target,
        &services.live,
        &services.edit,
        &services.ask,
    )
    .await?;
    recording_overlay::set_interactive(&app, false);
    if let Ok(mut language) = services.voice_translation_target.lock() {
        language.take();
    }
    if !cancelled {
        return Ok(());
    }
    if services.lifecycle.phase() == PipelinePhase::Idle {
        emit_state(
            &app,
            &state,
            AppPhase::Idle,
            if mode == PipelineMode::Translate {
                "Translation cancelled."
            } else if mode == PipelineMode::Edit {
                "Editing cancelled."
            } else if mode == PipelineMode::Ask {
                "Ask cancelled."
            } else {
                "Dictation cancelled."
            },
        );
    }
    Ok(())
}

async fn cancel_pipeline_operation(
    lifecycle: &PipelineLifecycle,
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    target: &Mutex<Option<TargetWindow>>,
    live: &tokio::sync::Mutex<Option<live_dictation::LiveTask>>,
    edit: &tokio::sync::Mutex<Option<EditSession>>,
    ask: &tokio::sync::Mutex<Option<AskSession>>,
) -> Result<bool, String> {
    let Some((operation_id, phase)) = lifecycle.cancel()? else {
        return Ok(false);
    };
    // Starting and Processing already have an owner. Releasing their lifecycle
    // here would allow a new recording to race their remaining writes/cleanup.
    if phase != PipelinePhase::Recording {
        return Ok(true);
    }
    let _guard = PipelineGuard {
        lifecycle,
        id: operation_id,
    };
    if let Ok(mut target) = target.lock() {
        target.take();
    }
    let live_task = live.lock().await.take();
    edit.lock().await.take();
    ask.lock().await.take();
    let audio_result = {
        let mut audio = audio.lock().await;
        audio.cancel().await
    };
    if let Some(task) = live_task {
        drop(task.finish().await?);
    }
    audio_result.map_err(command_error)?;
    Ok(true)
}

fn correction_failure_status(error: &correction::CorrectionError) -> String {
    let kind = match error {
        correction::CorrectionError::MissingApiKey(_) => "missing_api_key",
        correction::CorrectionError::Request(_) => "request_failed",
        correction::CorrectionError::Api { status, .. } => {
            return format!(
                "AI correction failed; using the original transcript. HTTP status {}.",
                status.as_u16()
            );
        }
        correction::CorrectionError::InvalidResponse(_) => "invalid_response",
        correction::CorrectionError::Cancelled => "cancelled",
        correction::CorrectionError::InvalidEndpoint(_) => "invalid_endpoint",
        correction::CorrectionError::UnsupportedProvider(_) => "unsupported_provider",
        correction::CorrectionError::EmptyEditInstruction => "empty_edit_instruction",
    };
    format!("AI correction failed; using the original transcript. Error kind: {kind}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioArtifact, AudioError, AudioFuture, CaptureState, LevelMeter};

    #[test]
    fn retry_dictate_label_tracks_correction_even_when_text_is_unchanged() {
        assert_eq!(retry_dictate_history_mode(Some("local")), "ai_corrected");
        assert_eq!(retry_dictate_history_mode(None), "faithful");
    }

    #[test]
    fn history_database_failure_is_reported_without_failing_completed_output() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("history-save-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let database = dir.join("test.db");
        let storage = Storage::open(&database).unwrap();
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute("DROP TABLE dictation_history", [])
            .unwrap();
        let status = save_history(
            &storage,
            &NewHistoryItem {
                transcript_text: "completed output",
                processed_text: None,
                source_text: None,
                instruction_text: None,
                action_kind: None,
                search_site: None,
                mode: "faithful",
                asr_provider: "test",
                llm_provider: None,
                target_language: None,
                app_category: None,
                duration_ms: None,
                latency_ms: None,
                retry_of_id: None,
            },
            None,
        );
        assert_eq!(status, HistorySaveStatus::Failed);
        drop(storage);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn settings_history_item() -> NewHistoryItem<'static> {
        NewHistoryItem {
            transcript_text: "private transcript",
            processed_text: None,
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: "faithful",
            asr_provider: "test",
            llm_provider: None,
            target_language: None,
            app_category: None,
            duration_ms: None,
            latency_ms: None,
            retry_of_id: None,
        }
    }

    fn stored_history_rows(database: &std::path::Path) -> i64 {
        rusqlite::Connection::open(database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM dictation_history", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn update_settings_persistence_purges_history_in_the_settings_write() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("settings.sqlite");
        let storage = Storage::open(&database).unwrap();
        assert!(
            storage
                .add_history_with_audio_report(&settings_history_item(), None)
                .unwrap()
                .0
        );
        let settings = Settings {
            history_retention: types::HistoryRetention::Never,
            ..Settings::default()
        };
        let mut rolled_back = false;

        persist_settings_or_rollback(&storage, &settings, || rolled_back = true).unwrap();

        assert!(!rolled_back);
        assert_eq!(
            storage.get_settings().unwrap().history_retention,
            types::HistoryRetention::Never
        );
        assert_eq!(stored_history_rows(&database), 0);
    }

    #[test]
    fn failed_update_settings_write_keeps_history_and_rolls_back_shortcuts() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("settings.sqlite");
        let storage = Storage::open(&database).unwrap();
        assert!(
            storage
                .add_history_with_audio_report(&settings_history_item(), None)
                .unwrap()
                .0
        );
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_settings_insert BEFORE INSERT ON settings
                 BEGIN SELECT RAISE(ABORT, 'settings write rejected'); END;
                 CREATE TRIGGER reject_settings_update BEFORE UPDATE ON settings
                 BEGIN SELECT RAISE(ABORT, 'settings write rejected'); END;",
            )
            .unwrap();
        let settings = Settings {
            history_retention: types::HistoryRetention::Never,
            ..Settings::default()
        };
        let mut rolled_back = false;

        assert!(persist_settings_or_rollback(&storage, &settings, || rolled_back = true).is_err());

        // The purge shares the failed settings transaction, so History-off
        // cannot erase rows while the stored settings stay unchanged.
        assert!(rolled_back);
        assert_eq!(stored_history_rows(&database), 1);
        assert_eq!(
            storage.get_settings().unwrap().history_retention,
            Settings::default().history_retention
        );
    }

    #[test]
    fn edit_persistence_failure_does_not_skip_metrics_or_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit.sqlite");
        let storage = Storage::open(&path).unwrap();
        rusqlite::Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE dictation_history", [])
            .unwrap();
        let item = NewHistoryItem {
            transcript_text: "shorten this",
            processed_text: Some("short"),
            source_text: Some("long selection"),
            instruction_text: Some("shorten this"),
            action_kind: None,
            search_site: None,
            mode: "edit",
            asr_provider: "test",
            llm_provider: Some("local"),
            target_language: None,
            app_category: None,
            duration_ms: Some(100),
            latency_ms: Some(200),
            retry_of_id: None,
        };
        assert_eq!(
            persist_edit_completion(&storage, &item, None, "local", 200),
            (HistorySaveStatus::Failed, false)
        );
    }

    #[test]
    fn persistence_failures_choose_one_complete_warning() {
        assert_eq!(persistence_warning(false, false), None);
        assert_eq!(
            persistence_warning(true, false).unwrap().0,
            "history_save_failed"
        );
        assert_eq!(
            persistence_warning(false, true).unwrap().0,
            "metric_save_failed"
        );
        assert_eq!(
            persistence_warning(true, true).unwrap().0,
            "history_and_metric_save_failed"
        );
        assert_eq!(
            completion_persistence_warning(HistorySaveStatus::AudioUnavailable, true)
                .unwrap()
                .0,
            "history_audio_and_metric_save_failed"
        );
        assert_eq!(
            completion_persistence_warning(HistorySaveStatus::AudioUnavailable, false)
                .unwrap()
                .0,
            "history_audio_unavailable"
        );
    }

    #[test]
    fn hotkey_repair_uses_only_confirmed_registrations() {
        let dictate = parse_shortcut("CommandOrControl+Shift+D").unwrap();
        let translate = parse_shortcut("CommandOrControl+Shift+T").unwrap();
        let selected = parse_shortcut("CommandOrControl+Shift+Y").unwrap();
        let unavailable = parse_shortcut("CommandOrControl+Shift+U").unwrap();
        let edit = parse_shortcut("CommandOrControl+Shift+E").unwrap();
        let ask = parse_shortcut("CommandOrControl+Shift+A").unwrap();
        let registered = vec![dictate, selected, edit, ask];
        let unchanged = vec![dictate, unavailable, selected, edit, ask];
        assert_eq!(
            hotkey_changes(&registered, &unchanged),
            (vec![], vec![unavailable])
        );
        assert!(!hotkeys_repaired(&registered, &unchanged));

        let repaired = vec![dictate, translate, selected, edit, ask];
        assert_eq!(
            hotkey_changes(&registered, &repaired),
            (vec![], vec![translate])
        );
        assert!(hotkeys_repaired(&repaired, &repaired));
    }

    #[test]
    fn translation_targets_require_supported_unique_current_language() {
        let mut settings = Settings::default();
        assert!(validate_translation_targets(&settings).is_ok());

        settings.translation_target_languages = vec!["en".into(), "en".into()];
        assert!(validate_translation_targets(&settings).is_err());

        settings.translation_target_languages = vec!["xx".into()];
        settings.translation_target_language = "xx".into();
        assert!(validate_translation_targets(&settings).is_err());

        settings.translation_target_languages = vec!["ja".into()];
        settings.translation_target_language = "en".into();
        assert!(validate_translation_targets(&settings).is_err());
    }

    struct TestAudio {
        cancel_error: bool,
    }

    impl AudioCapture for TestAudio {
        fn snapshot(&self, _: Duration) -> Result<crate::audio::AudioSnapshot, AudioError> {
            Err(AudioError::NotCapturing)
        }

        fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn arm(&mut self, _config: CaptureConfig) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn disarm(&mut self) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn start(&mut self, _config: CaptureConfig) -> AudioFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
            Box::pin(async { Err(AudioError::NotCapturing) })
        }

        fn cancel(&mut self) -> AudioFuture<'_, ()> {
            let cancel_error = self.cancel_error;
            Box::pin(async move {
                if cancel_error {
                    Err(AudioError::NotCapturing)
                } else {
                    Ok(())
                }
            })
        }

        fn state(&self) -> CaptureState {
            CaptureState::Idle
        }

        fn level(&self) -> LevelMeter {
            LevelMeter::default()
        }
    }

    fn test_audio(cancel_error: bool) -> tokio::sync::Mutex<Box<dyn AudioCapture>> {
        tokio::sync::Mutex::new(Box::new(TestAudio { cancel_error }))
    }

    #[tokio::test]
    async fn immediate_stop_waits_for_session_publication() {
        let live = Arc::new(tokio::sync::Mutex::new(None));
        let edit = Arc::new(tokio::sync::Mutex::new(None));
        let ask = Arc::new(tokio::sync::Mutex::new(None));
        let translation_target = Arc::new(Mutex::new(None));
        let mut startup_publication = live.lock().await;
        let live_for_stop = Arc::clone(&live);
        let edit_for_stop = Arc::clone(&edit);
        let ask_for_stop = Arc::clone(&ask);
        let target_for_stop = Arc::clone(&translation_target);
        let stopping = tokio::spawn(async move {
            take_published_sessions(
                &live_for_stop,
                &edit_for_stop,
                &ask_for_stop,
                &target_for_stop,
            )
            .await
            .unwrap()
        });

        tokio::task::yield_now().await;
        *translation_target.lock().unwrap() = Some("ja".into());
        *startup_publication = Some(());
        drop(startup_publication);

        let (session, edit_session, ask_session, target) = stopping.await.unwrap();
        assert_eq!(session, Some(()));
        assert_eq!(edit_session, None::<()>);
        assert!(ask_session.is_none());
        assert_eq!(target.as_deref(), Some("ja"));
    }

    #[tokio::test]
    async fn cancel_waits_for_audio_operation_then_allows_new_start() {
        let lifecycle = PipelineLifecycle::default();
        let operation_id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.mark_recording(operation_id).unwrap();
        let audio = test_audio(false);
        let target = Mutex::new(None);
        let audio_operation = audio.lock().await;
        let live = tokio::sync::Mutex::new(None);
        let edit = tokio::sync::Mutex::new(None);
        let ask = tokio::sync::Mutex::new(None);
        let cancellation =
            cancel_pipeline_operation(&lifecycle, &audio, &target, &live, &edit, &ask);
        tokio::pin!(cancellation);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), cancellation.as_mut())
                .await
                .is_err()
        );
        assert!(
            !cancel_pipeline_operation(&lifecycle, &audio, &target, &live, &edit, &ask)
                .await
                .unwrap()
        );
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        drop(audio_operation);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), cancellation)
                .await
                .unwrap(),
            Ok(true)
        );
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[tokio::test]
    async fn failed_audio_cancel_still_allows_new_start() {
        let lifecycle = PipelineLifecycle::default();
        let operation_id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.mark_recording(operation_id).unwrap();
        let audio = test_audio(true);
        let target = Mutex::new(None);

        assert!(cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None)
        )
        .await
        .is_err());
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[tokio::test]
    async fn processing_cancel_retains_ownership_until_processing_exits() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        let (_, _, cancel) = lifecycle.begin_processing().unwrap();
        cancel_pipeline_operation(
            &lifecycle,
            &test_audio(false),
            &Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
        )
        .await
        .unwrap();
        assert!(*cancel.borrow());
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_ok());
    }

    #[test]
    fn correction_failure_status_excludes_provider_response_body() {
        let marker = "private transcript echoed by provider";
        let message = correction_failure_status(&correction::CorrectionError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: marker.into(),
        });

        assert_eq!(
            message,
            "AI correction failed; using the original transcript. HTTP status 400."
        );
        assert!(!message.contains(marker));
    }
}

#[tauri::command]
pub(crate) fn get_settings(storage: State<'_, Storage>) -> Result<Settings, String> {
    storage.get_settings().map_err(command_error)
}

#[tauri::command]
pub(crate) async fn update_settings(
    app: AppHandle,
    settings: Settings,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<Settings, String> {
    if !["en", "ja"].contains(&settings.ui_language.as_str()) {
        return Err("ui language must be en or ja".into());
    }
    if settings.hotkey.trim().is_empty() {
        return Err("hotkey cannot be empty".into());
    }
    if settings.translation_hotkey.trim().is_empty() {
        return Err("translation hotkey cannot be empty".into());
    }
    if settings.voice_translate_hotkey.trim().is_empty() {
        return Err("voice Translate hotkey cannot be empty".into());
    }
    if settings.speak_to_edit_hotkey.trim().is_empty() {
        return Err("Speak to edit hotkey cannot be empty".into());
    }
    if settings.ask_hotkey.trim().is_empty() {
        return Err("Ask hotkey cannot be empty".into());
    }
    validate_translation_targets(&settings)?;
    if !["off", "low", "medium", "high"].contains(&settings.noise_suppression.as_str()) {
        return Err("noise suppression must be off, low, medium, or high".into());
    }
    if !(25..=400).contains(&settings.input_gain_percent) {
        return Err("input gain must be between 25 and 400 percent".into());
    }
    if !types::ASR_BACKENDS.contains(&settings.asr_backend.as_str()) {
        return Err(
            "asr backend must be vibevoice, faster-whisper, openai-compatible, or mock".into(),
        );
    }
    if !types::CORRECTION_PROVIDERS.contains(&settings.correction_provider.as_str()) {
        return Err("text correction provider must be openai, gemini, or local".into());
    }
    if !types::OPENAI_REASONING_EFFORTS.contains(&settings.openai_reasoning_effort.as_str()) {
        return Err(
            "OpenAI reasoning effort must be none, low, medium, high, xhigh, or max".into(),
        );
    }
    if !["4bit", "8bit", "bf16"].contains(&settings.model_quantization.as_str()) {
        return Err("model quantization must be 4bit, 8bit, or bf16".into());
    }
    if settings.model_id.as_deref().is_some_and(|value| {
        value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control)
    }) {
        return Err(
            "model ID must be non-empty, at most 512 characters, and contain no control characters"
                .into(),
        );
    }
    if settings.custom_models.len() > 100 {
        return Err("at most 100 custom models can be saved".into());
    }
    let api_url = settings.api_base_url.trim();
    if api_url.len() > 2048
        || !(api_url.starts_with("http://") || api_url.starts_with("https://"))
        || api_url.chars().any(char::is_control)
    {
        return Err(
            "API base URL must be an absolute HTTP(S) URL of at most 2048 characters".into(),
        );
    }
    let api_key_env = settings.api_key_env_var.trim();
    if api_key_env.is_empty()
        || api_key_env.len() > 128
        || !api_key_env
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(
            "API key environment variable must contain only ASCII letters, digits, or underscores"
                .into(),
        );
    }
    for (label, model) in [
        (
            "OpenAI correction model",
            settings.openai_correction_model.as_str(),
        ),
        (
            "Gemini correction model",
            settings.gemini_correction_model.as_str(),
        ),
        (
            "Local correction model",
            settings.local_correction_model.as_str(),
        ),
    ] {
        if model.trim().is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
            return Err(format!(
                "{label} must be non-empty, at most 512 characters, and contain no control characters"
            ));
        }
    }
    correction::local_chat_completions_url(&settings.local_correction_base_url)
        .map_err(|error| error.to_string())?;
    if !(128..=32768).contains(&settings.local_correction_max_tokens) {
        return Err("local correction max tokens must be between 128 and 32768".into());
    }
    for (label, environment_variable) in [
        (
            "OpenAI API key environment variable",
            settings.openai_api_key_env_var.as_str(),
        ),
        (
            "Gemini API key environment variable",
            settings.gemini_api_key_env_var.as_str(),
        ),
    ] {
        let environment_variable = environment_variable.trim();
        if environment_variable.is_empty()
            || environment_variable.len() > 128
            || !environment_variable
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
        {
            return Err(format!(
                "{label} must contain only ASCII letters, digits, or underscores"
            ));
        }
    }
    if settings.correction_instruction.chars().count() > 500
        || settings.correction_instruction.chars().any(|character| {
            character.is_control() && character != '\n' && character != '\r' && character != '\t'
        })
    {
        return Err(
            "correction instruction must be at most 500 characters and contain no unsupported control characters"
                .into(),
        );
    }
    for model in &settings.custom_models {
        if !types::ASR_BACKENDS.contains(&model.asr_backend.as_str()) {
            return Err("custom model backend is not supported".into());
        }
        if model.model_id.trim().is_empty()
            || model.model_id.len() > 512
            || model.model_id.chars().any(char::is_control)
        {
            return Err(
                "custom model ID must be non-empty, at most 512 characters, and contain no control characters"
                    .into(),
            );
        }
    }
    let new_shortcut = parse_shortcut(&settings.hotkey)?;
    let new_translation_shortcut = parse_shortcut(&settings.translation_hotkey)?;
    let new_voice_translate_shortcut = parse_shortcut(&settings.voice_translate_hotkey)?;
    let new_edit_shortcut = parse_shortcut(&settings.speak_to_edit_hotkey)?;
    let new_ask_shortcut = parse_shortcut(&settings.ask_hotkey)?;
    let previous = storage.get_settings().map_err(command_error)?;
    if new_translation_shortcut == new_shortcut
        || new_voice_translate_shortcut == new_shortcut
        || new_voice_translate_shortcut == new_translation_shortcut
        || new_edit_shortcut == new_shortcut
        || new_edit_shortcut == new_translation_shortcut
        || new_edit_shortcut == new_voice_translate_shortcut
        || new_ask_shortcut == new_shortcut
        || new_ask_shortcut == new_translation_shortcut
        || new_ask_shortcut == new_voice_translate_shortcut
        || new_ask_shortcut == new_edit_shortcut
    {
        return Err(
            "recording, selected-text translation, voice Translate, Speak to edit, and Ask hotkeys must differ".into(),
        );
    }
    if settings.translation_instruction.chars().count() > 500
        || settings.translation_instruction.chars().any(|character| {
            character.is_control() && character != '\n' && character != '\r' && character != '\t'
        })
    {
        return Err("translation instruction must be at most 500 characters and contain no unsupported control characters".into());
    }
    if cfg!(debug_assertions) && settings.auto_start && !previous.auto_start {
        return Err(
            "autostart cannot be enabled from a development build; install and run a release build"
                .into(),
        );
    }
    let new_shortcuts = unique_shortcuts([
        new_shortcut,
        new_voice_translate_shortcut,
        new_edit_shortcut,
        new_ask_shortcut,
        new_translation_shortcut,
    ]);
    let repaired = {
        let mut registered = services
            .registered_hotkeys
            .lock()
            .map_err(|_| "hotkey registration state is unavailable".to_string())?;
        let (to_remove, to_add) = hotkey_changes(&registered, &new_shortcuts);
        let mut removed = Vec::new();
        let mut added = Vec::new();
        let rollback_shortcuts =
            |removed: &[Shortcut], added: &[Shortcut], registered: &mut Vec<Shortcut>| {
                for shortcut in added.iter().rev() {
                    if app.global_shortcut().unregister(*shortcut).is_ok() {
                        registered.retain(|active| active != shortcut);
                    }
                }
                for shortcut in removed {
                    if app.global_shortcut().register(*shortcut).is_ok() {
                        registered.push(*shortcut);
                    }
                }
            };
        for shortcut in to_remove {
            if let Err(error) = app.global_shortcut().unregister(shortcut) {
                rollback_shortcuts(&removed, &added, &mut registered);
                return Err(format!("hotkey update failed: {error}"));
            }
            registered.retain(|active| *active != shortcut);
            removed.push(shortcut);
        }
        for shortcut in to_add {
            if let Err(error) = app.global_shortcut().register(shortcut) {
                rollback_shortcuts(&removed, &added, &mut registered);
                return Err(format!("hotkey registration failed: {error}"));
            }
            registered.push(shortcut);
            added.push(shortcut);
        }
        persist_settings_or_rollback(&storage, &settings, || {
            rollback_shortcuts(&removed, &added, &mut registered)
        })?;
        hotkeys_repaired(&registered, &new_shortcuts)
    };
    if repaired {
        if let Ok(mut issues) = services.startup_hotkey_issues.lock() {
            *issues = StartupHotkeyIssues::default();
        }
    }
    if settings.auto_start != previous.auto_start {
        let result = if settings.auto_start {
            app.autolaunch().enable()
        } else {
            app.autolaunch().disable()
        };
        if let Err(error) = result {
            emit_status(
                &app,
                "autostart_update_failed",
                &format!("Autostart update failed: {error}"),
            );
        }
    }
    let model_configuration_changed = settings.asr_backend != previous.asr_backend
        || settings.model_id != previous.model_id
        || settings.api_base_url != previous.api_base_url
        || settings.api_key_env_var != previous.api_key_env_var;
    if model_configuration_changed {
        services
            .transcriber
            .reconfigure(worker_command_for_settings(&settings))
            .await;
    }
    if model_configuration_changed || settings.model_quantization != previous.model_quantization {
        let (model_id, detail) = model_identity(&settings);
        let status = ModelStatus {
            model_id,
            installed: false,
            state: "not_loaded".into(),
            detail,
        };
        *services
            .model
            .lock()
            .map_err(|_| "model service is unavailable".to_string())? = status.clone();
        let _ = app.emit("model-status", status);
    }
    Ok(settings)
}

#[tauri::command]
pub(crate) fn list_history(
    filter: Option<types::HistoryFilter>,
    limit: Option<u32>,
    storage: State<'_, Storage>,
) -> Result<Vec<HistoryItem>, String> {
    storage
        .list_history(
            filter.unwrap_or(types::HistoryFilter::All),
            limit.unwrap_or(100),
        )
        .map_err(command_error)
}

#[tauri::command]
pub(crate) fn delete_history_item(id: i64, storage: State<'_, Storage>) -> Result<bool, String> {
    storage.delete_history(id).map_err(command_error)
}

#[tauri::command]
pub(crate) fn delete_all_history(storage: State<'_, Storage>) -> Result<u64, String> {
    storage.delete_all_history().map_err(command_error)
}

#[tauri::command]
pub(crate) fn get_history_audio(
    id: i64,
    storage: State<'_, Storage>,
) -> Result<types::HistoryAudioPayload, String> {
    storage
        .history_audio(id)
        .map_err(command_error)?
        .ok_or_else(|| "history audio is unavailable".into())
}

#[tauri::command]
pub(crate) async fn retry_history_item(
    id: i64,
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<RecordingResult, String> {
    let source = storage
        .history_item(id)
        .map_err(command_error)?
        .ok_or_else(|| "history item not found".to_string())?;
    let mode = history_pipeline_mode(&source.mode)?;
    let (operation_id, cancel) = services
        .lifecycle
        .begin_retry(mode)
        .map_err(str::to_string)?;
    let _guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    let mut retry_cleanup = None;
    let result = async {
        emit_state(
            &app,
            &state,
            AppPhase::Processing,
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
        let prompt_terms = storage.dictionary_prompt_terms().unwrap_or_default();
        let prompt = (!prompt_terms.is_empty()).then(|| prompt_terms.join("\n"));
        let transcript = services
            .transcriber
            .transcribe(&retry_path, prompt.as_deref(), cancel.clone())
            .await
            .map_err(command_error)?;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let mut output = transcript.text.clone();
        let mut llm_provider = None;
        if source.mode == "translate" {
            let target = source
                .target_language
                .as_deref()
                .ok_or("saved translation target is unavailable")?;
            output = correction::translate_transcript(
                &settings,
                &transcript.text,
                target,
                cancel.clone(),
                |_| {},
            )
            .await
            .map_err(command_error)?;
            llm_provider = Some(settings.correction_provider.as_str());
        } else if source.mode == "edit" {
            let selected = source
                .source_text
                .as_deref()
                .ok_or("saved edit source is unavailable")?;
            output = correction::edit_selected_text(
                &settings,
                selected,
                &transcript.text,
                cancel.clone(),
                |_| {},
            )
            .await
            .map_err(command_error)?;
            llm_provider = Some(settings.correction_provider.as_str());
        } else if source.mode == "ask" {
            if source.action_kind.as_deref() == Some("search") {
                let stored_site = stored_search_site(&source)?;
                let plan = correction::generate_ask_plan(
                    &settings,
                    &transcript.text,
                    &ask::planning_prompt(ask::AskContextKind::Caret),
                    cancel.clone(),
                )
                .await
                .map_err(|_| "Ask search retry planning failed".to_string())?;
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
                output = correction::generate_ask_text(
                    &settings,
                    &input,
                    &ask::generation_prompt_with_target(&action),
                    cancel.clone(),
                )
                .await
                .map_err(command_error)?;
            }
            llm_provider = Some(settings.correction_provider.as_str());
        } else if settings.text_correction_enabled {
            let hints = storage
                .dictionary_correction_hints(&transcript.text)
                .unwrap_or_default();
            output = correction::correct_transcript(
                &settings,
                &transcript.text,
                &hints,
                cancel.clone(),
                |_| {},
            )
            .await
            .map_err(command_error)?;
            llm_provider = Some(settings.correction_provider.as_str());
        }
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let latency_ms = started.elapsed().as_millis() as i64;
        let history_save_status = services
            .lifecycle
            .commit_retry(operation_id, || {
                Ok::<HistorySaveStatus, String>(save_history(
                    &storage,
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
                            retry_dictate_history_mode(llm_provider)
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
                    },
                    Some(&retry_path),
                ))
            })
            .map_err(str::to_string)?
            .map_err(command_error)?;
        recording_overlay::set_phase(&app, &AppPhase::Completed);
        let snapshot = state.complete(
            output.clone(),
            "History retry completed without inserting text.".into(),
        );
        let _ = app.emit("app-state", snapshot);
        report_history_save_warning(&app, history_save_status);
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
            &app,
            "artifact_cleanup_failed",
            "Temporary retry audio cleanup failed.",
        );
    }
    if result.is_err() {
        let message = if services.lifecycle.is_cancelled(operation_id) {
            "History retry cancelled."
        } else {
            "History retry failed."
        };
        emit_state(
            &app,
            &state,
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

fn history_pipeline_mode(mode: &str) -> Result<PipelineMode, String> {
    match mode {
        "translate" => Ok(PipelineMode::Translate),
        "edit" => Ok(PipelineMode::Edit),
        "ask" => Ok(PipelineMode::Ask),
        "faithful" | "ai_corrected" | "faithful_fallback" => Ok(PipelineMode::Dictate),
        _ => Err("history mode is not retryable".into()),
    }
}

fn retry_dictate_history_mode(llm_provider: Option<&str>) -> &'static str {
    if llm_provider.is_some() {
        "ai_corrected"
    } else {
        "faithful"
    }
}

fn stored_ask_action(item: &HistoryItem) -> Result<ask::AskAction, String> {
    Ok(match item.action_kind.as_deref() {
        Some("rewrite") => ask::AskAction::Rewrite,
        Some("shorten") => ask::AskAction::Shorten,
        Some("expand") => ask::AskAction::Expand,
        Some("change_tone") => ask::AskAction::ChangeTone,
        Some("summarize") => ask::AskAction::Summarize,
        Some("explain") => ask::AskAction::Explain,
        Some("answer") => ask::AskAction::Answer,
        Some("draft") => ask::AskAction::Draft,
        Some("translate") => ask::AskAction::Translate {
            target_language: item
                .target_language
                .clone()
                .ok_or("saved Ask translation target is unavailable")?,
        },
        _ => return Err("saved Ask action is unavailable".into()),
    })
}

fn stored_search_site(item: &HistoryItem) -> Result<ask::SearchSite, String> {
    match item.search_site.as_deref() {
        Some("google") => Ok(ask::SearchSite::Google),
        Some("youtube") => Ok(ask::SearchSite::YouTube),
        Some("amazon_japan") => Ok(ask::SearchSite::AmazonJapan),
        Some("github") => Ok(ask::SearchSite::GitHub),
        _ => Err("saved Ask search site is unavailable".into()),
    }
}

#[tauri::command]
pub(crate) fn list_dictionary(storage: State<'_, Storage>) -> Result<Vec<DictionaryEntry>, String> {
    storage.list_dictionary().map_err(command_error)
}

#[tauri::command]
pub(crate) fn add_dictionary_entry(
    entry: DictionaryEntryInput,
    storage: State<'_, Storage>,
) -> Result<DictionaryEntry, String> {
    let id = storage
        .add_dictionary_entry(&NewDictionaryEntry {
            reading: &entry.reading,
            surface: &entry.surface,
            category: entry.category.as_deref(),
            aliases: &entry.aliases,
            priority: entry.priority,
            app_scope: entry.app_scope.as_deref(),
        })
        .map_err(command_error)?;
    storage
        .list_dictionary()
        .map_err(command_error)?
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or_else(|| "dictionary entry not found".to_string())
}

#[tauri::command]
pub(crate) fn delete_dictionary_entry(id: i64, storage: State<'_, Storage>) -> Result<(), String> {
    if storage.delete_dictionary_entry(id).map_err(command_error)? {
        Ok(())
    } else {
        Err("dictionary entry not found".into())
    }
}

#[tauri::command]
pub(crate) fn copy_history_item(id: i64, storage: State<'_, Storage>) -> Result<(), String> {
    let text = storage
        .history_text(id)
        .map_err(command_error)?
        .ok_or_else(|| "history item not found".to_string())?;
    #[cfg(windows)]
    clipboard_win::set_clipboard_string(&text)
        .map_err(|_| "clipboard operation failed".to_string())?;
    #[cfg(not(windows))]
    {
        let _ = text;
        return Err("clipboard copy is available in the Windows build".into());
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[tauri::command]
pub(crate) fn copy_to_clipboard(text: String) -> Result<(), String> {
    #[cfg(windows)]
    clipboard_win::set_clipboard_string(&text)
        .map_err(|_| "clipboard operation failed".to_string())?;
    #[cfg(not(windows))]
    {
        let _ = text;
        return Err("clipboard copy is available in the Windows build".into());
    }
    #[allow(unreachable_code)]
    Ok(())
}

#[tauri::command]
pub(crate) fn get_ask_answer(
    services: State<'_, Services>,
) -> Result<Option<answer_panel::AnswerPayload>, String> {
    Ok(services.answer_panel.current())
}

#[tauri::command]
pub(crate) fn dismiss_ask_answer(
    operation_id: u64,
    app: AppHandle,
    services: State<'_, Services>,
) -> Result<bool, String> {
    Ok(services.answer_panel.dismiss_with(operation_id, || {
        app.get_webview_window(answer_panel::WINDOW_LABEL)
            .is_some_and(|window| window.hide().is_ok())
    }))
}

#[tauri::command]
pub(crate) fn get_model_status(services: State<'_, Services>) -> Result<ModelStatus, String> {
    services
        .model
        .lock()
        .map(|status| status.clone())
        .map_err(|_| "model service is unavailable".into())
}

#[tauri::command]
pub(crate) async fn load_model(
    app: AppHandle,
    request: Option<LoadModelRequest>,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<ModelStatus, String> {
    let settings = storage.get_settings().map_err(command_error)?;
    if let Some(request) = request {
        let requested_model = request
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let configured_model = settings
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if requested_model != configured_model
            || request
                .quantization
                .as_deref()
                .is_some_and(|value| value != settings.model_quantization)
        {
            return Err("model settings changed; save them before loading".into());
        }
    }
    ensure_model_loaded(&app, &services, &settings).await
}

pub(crate) async fn ensure_model_loaded(
    app: &AppHandle,
    services: &Services,
    settings: &Settings,
) -> Result<ModelStatus, String> {
    let (model_id, detail) = model_identity(settings);
    if let Ok(status) = services.model.lock() {
        if status.state == "ready" && status.model_id == model_id {
            return Ok(status.clone());
        }
    }
    emit_status(app, "model_loading", "Loading the speech model.");
    let loading = ModelStatus {
        model_id: model_id.clone(),
        installed: false,
        state: "loading".into(),
        detail: detail.clone(),
    };
    *services
        .model
        .lock()
        .map_err(|_| "model service is unavailable".to_string())? = loading.clone();
    let _ = app.emit("model-status", loading);

    // Only the worker knows whether the model files are already cached, so the
    // download message and progress bar are driven by what it reports.
    let progress_forwarder = spawn_load_progress_forwarder(app, services);
    let load_result = services
        .transcriber
        .load(&settings.model_quantization)
        .await;
    if let Some(forwarder) = progress_forwarder {
        forwarder.abort();
    }

    if let Err(error) = load_result {
        let message = command_error(&error);
        let failed = ModelStatus {
            model_id,
            installed: false,
            state: "error".into(),
            detail: format!("{detail} {message}"),
        };
        if let Ok(mut status) = services.model.lock() {
            *status = failed.clone();
        }
        let _ = app.emit("model-status", failed);
        return Err(message);
    }
    let status = ModelStatus {
        model_id,
        installed: true,
        state: "ready".into(),
        detail: format!(
            "{detail} Loaded with {} quantization.",
            settings.model_quantization
        ),
    };
    *services
        .model
        .lock()
        .map_err(|_| "model service is unavailable".to_string())? = status.clone();
    let _ = app.emit("model-status", status.clone());
    Ok(status)
}

/// Relay worker load progress to the window until the load finishes.
///
/// The first download report also replaces the loading message, so a model that
/// is already cached never claims that files are being downloaded.
fn spawn_load_progress_forwarder(
    app: &AppHandle,
    services: &Services,
) -> Option<tauri::async_runtime::JoinHandle<()>> {
    let mut receiver = services.transcriber.load_progress()?;
    let app = app.clone();
    Some(tauri::async_runtime::spawn(async move {
        let mut download_announced = false;
        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    if progress.stage == "download" && !download_announced {
                        download_announced = true;
                        emit_status(
                            &app,
                            "model_downloading",
                            "Downloading the speech model files. This runs once; later starts use the local cache.",
                        );
                    }
                    let _ = app.emit("model-progress", progress);
                }
                // Progress is advisory: a dropped batch must not end reporting.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    }))
}

pub(crate) fn probe_gpu_diagnostics() -> GpuDiagnostics {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,driver_version,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output();
    if let Ok(output) = output {
        if output.status.success() {
            let line = String::from_utf8_lossy(&output.stdout);
            let values: Vec<&str> = line
                .lines()
                .next()
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .collect();
            if values.len() == 3 {
                let memory = values[2].parse::<u64>().ok();
                return GpuDiagnostics {
                    status: "available".into(),
                    adapter_name: Some(values[0].into()),
                    driver_version: Some(values[1].into()),
                    memory_total_mb: memory,
                    recommendation: if memory.unwrap_or_default() >= 12_000 {
                        "A CUDA GPU with 12 GB or more is suitable for VibeVoice; faster-whisper also runs without a GPU."
                    } else {
                        "VibeVoice may exceed available VRAM; a CUDA GPU with 12 GB or more is recommended only for VibeVoice. faster-whisper runs without a GPU."
                    }
                    .into(),
                };
            }
        }
    }
    GpuDiagnostics {
        status: "unavailable".into(),
        adapter_name: None,
        driver_version: None,
        memory_total_mb: None,
        recommendation: "NVIDIA diagnostics unavailable. A CUDA GPU with 12 GB or more is recommended only for VibeVoice; faster-whisper runs without a GPU.".into(),
    }
}

#[tauri::command]
pub(crate) async fn get_gpu_diagnostics(
    services: State<'_, Services>,
) -> Result<GpuDiagnostics, String> {
    if let Some(diagnostics) = services
        .gpu_diagnostics
        .lock()
        .map_err(|_| "GPU diagnostics are unavailable".to_string())?
        .clone()
    {
        return Ok(diagnostics);
    }

    let diagnostics = tauri::async_runtime::spawn_blocking(probe_gpu_diagnostics)
        .await
        .map_err(|_| "GPU diagnostics could not be completed".to_string())?;
    *services
        .gpu_diagnostics
        .lock()
        .map_err(|_| "GPU diagnostics are unavailable".to_string())? = Some(diagnostics.clone());
    Ok(diagnostics)
}

#[tauri::command]
pub(crate) async fn run_gpu_diagnostics(
    app: AppHandle,
    services: State<'_, Services>,
) -> Result<GpuDiagnostics, String> {
    let diagnostics = tauri::async_runtime::spawn_blocking(probe_gpu_diagnostics)
        .await
        .map_err(|_| "GPU diagnostics could not be completed".to_string())?;
    *services
        .gpu_diagnostics
        .lock()
        .map_err(|_| "GPU diagnostics are unavailable".to_string())? = Some(diagnostics.clone());
    let _ = app.emit("gpu-diagnostics", diagnostics.clone());
    Ok(diagnostics)
}

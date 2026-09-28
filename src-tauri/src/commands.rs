use super::*;

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
    artifact_cleanup.retain = !settings.delete_audio_after_processing;
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
    let cleanup_result = artifact_cleanup.cleanup();
    let transcript = match transcript_result {
        Ok(value) => value,
        Err(asr::AsrError::Cancelled) => {
            if cleanup_result.is_err() {
                emit_status(
                    &app,
                    "artifact_cleanup_failed",
                    "Temporary audio cleanup failed.",
                );
            }
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
            if cleanup_result.is_err() {
                emit_status(
                    &app,
                    "artifact_cleanup_failed",
                    "Temporary audio cleanup failed.",
                );
            }
            return Err(command_error(error));
        }
    };
    if cleanup_result.is_err() {
        emit_status(
            &app,
            "artifact_cleanup_failed",
            "Temporary audio cleanup failed.",
        );
    }
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
        storage
            .add_history(&NewHistoryItem {
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
            })
            .map_err(command_error)?;
        storage
            .add_metric(
                "speak_to_edit",
                Some(settings.correction_provider.as_str()),
                Some(edit_started.elapsed().as_millis() as i64),
                true,
                None,
            )
            .map_err(command_error)?;
        let (insertion_label, completion) = match insertion {
            InsertResult::ClipboardPaste => ("clipboard_paste", "Selected text updated."),
            InsertResult::ClipboardOnly => (
                "clipboard_only",
                "The original selection changed; the edit remains on the clipboard.",
            ),
            InsertResult::PasteUnverified => (
                "paste_unverified",
                "The edit paste could not be confirmed; the result remains on the clipboard.",
            ),
        };
        let snapshot = state.complete(edited.clone(), completion.into());
        let _ = app.emit("app-state", snapshot);
        emit_status(&app, insertion_label, completion);
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
    storage
        .add_history(&NewHistoryItem {
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
        })
        .map_err(command_error)?;
    storage
        .add_metric(
            if mode == PipelineMode::Translate {
                "voice_translate"
            } else {
                "dictation"
            },
            Some(&transcript.model),
            Some(latency_ms as i64),
            !translation_failed,
            translation_failed.then_some("translation_failed"),
        )
        .map_err(command_error)?;
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
        if storage
            .add_history(&NewHistoryItem {
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
            })
            .is_err()
        {
            emit_status(
                app,
                "history_save_failed",
                "Search opened, but History could not be saved.",
            );
        }
        let snapshot = state.complete(query.into(), "Opening the requested fixed search.".into());
        let _ = app.emit("app-state", snapshot);
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
    if storage
        .add_history(&NewHistoryItem {
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
        })
        .is_err()
    {
        emit_status(
            app,
            "history_save_failed",
            "Ask completed, but History could not be saved.",
        );
    }
    let message = if insertion == "paste_unverified" {
        "Ask insertion could not be confirmed; the result remains on the clipboard."
    } else if insertion == "clipboard_only" {
        "Ask result is on the clipboard."
    } else {
        "Ask completed."
    };
    let snapshot = state.complete(output.clone(), message.into());
    let _ = app.emit("app-state", snapshot);
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
    };
    format!("AI correction failed; using the original transcript. Error kind: {kind}.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{AudioArtifact, AudioError, AudioFuture, CaptureState, LevelMeter};

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
    if !(1..=3650).contains(&settings.history_retention_days) {
        return Err("history retention must be between 1 and 3650 days".into());
    }
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
    let old_shortcut = parse_shortcut(&previous.hotkey)?;
    let old_translation_shortcut = parse_shortcut(&previous.translation_hotkey)?;
    let old_voice_translate_shortcut = parse_shortcut(&previous.voice_translate_hotkey)?;
    let old_edit_shortcut = parse_shortcut(&previous.speak_to_edit_hotkey)?;
    let old_ask_shortcut = parse_shortcut(&previous.ask_hotkey)?;
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
            "recording, selected-text translation, voice Translate, and Speak to edit hotkeys must differ".into(),
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
    let recording_changed = new_shortcut != old_shortcut;
    let translation_changed = new_translation_shortcut != old_translation_shortcut;
    let voice_translate_changed = new_voice_translate_shortcut != old_voice_translate_shortcut;
    let edit_changed = new_edit_shortcut != old_edit_shortcut;
    let ask_changed = new_ask_shortcut != old_ask_shortcut;
    let rollback_shortcuts = || {
        if recording_changed {
            let _ = app.global_shortcut().unregister(new_shortcut);
        }
        if translation_changed {
            let _ = app.global_shortcut().unregister(new_translation_shortcut);
        }
        if voice_translate_changed {
            let _ = app
                .global_shortcut()
                .unregister(new_voice_translate_shortcut);
        }
        if edit_changed {
            let _ = app.global_shortcut().unregister(new_edit_shortcut);
        }
        if ask_changed {
            let _ = app.global_shortcut().unregister(new_ask_shortcut);
        }
        if recording_changed {
            let _ = app.global_shortcut().register(old_shortcut);
        }
        if translation_changed {
            let _ = app.global_shortcut().register(old_translation_shortcut);
        }
        if voice_translate_changed {
            let _ = app.global_shortcut().register(old_voice_translate_shortcut);
        }
        if edit_changed {
            let _ = app.global_shortcut().register(old_edit_shortcut);
        }
        if ask_changed {
            let _ = app.global_shortcut().register(old_ask_shortcut);
        }
    };
    // Remove every changed registration before adding any replacement. This
    // makes swapping the four shortcuts an atomic-looking transaction.
    if recording_changed {
        if let Err(error) = app.global_shortcut().unregister(old_shortcut) {
            rollback_shortcuts();
            return Err(format!("hotkey update failed: {error}"));
        }
    }
    if translation_changed {
        if let Err(error) = app.global_shortcut().unregister(old_translation_shortcut) {
            rollback_shortcuts();
            return Err(format!("translation hotkey update failed: {error}"));
        }
    }
    if voice_translate_changed {
        if let Err(error) = app
            .global_shortcut()
            .unregister(old_voice_translate_shortcut)
        {
            rollback_shortcuts();
            return Err(format!("voice Translate hotkey update failed: {error}"));
        }
    }
    if edit_changed {
        if let Err(error) = app.global_shortcut().unregister(old_edit_shortcut) {
            rollback_shortcuts();
            return Err(format!("Speak to edit hotkey update failed: {error}"));
        }
    }
    if ask_changed {
        if let Err(error) = app.global_shortcut().unregister(old_ask_shortcut) {
            rollback_shortcuts();
            return Err(format!("Ask hotkey update failed: {error}"));
        }
    }
    if recording_changed {
        if let Err(error) = app.global_shortcut().register(new_shortcut) {
            rollback_shortcuts();
            return Err(format!("hotkey registration failed: {error}"));
        }
    }
    if translation_changed {
        if let Err(error) = app.global_shortcut().register(new_translation_shortcut) {
            rollback_shortcuts();
            return Err(format!("translation hotkey registration failed: {error}"));
        }
    }
    if voice_translate_changed {
        if let Err(error) = app.global_shortcut().register(new_voice_translate_shortcut) {
            rollback_shortcuts();
            return Err(format!(
                "voice Translate hotkey registration failed: {error}"
            ));
        }
    }
    if edit_changed {
        if let Err(error) = app.global_shortcut().register(new_edit_shortcut) {
            rollback_shortcuts();
            return Err(format!("Speak to edit hotkey registration failed: {error}"));
        }
    }
    if ask_changed {
        if let Err(error) = app.global_shortcut().register(new_ask_shortcut) {
            rollback_shortcuts();
            return Err(format!("Ask hotkey registration failed: {error}"));
        }
    }
    if let Err(error) = storage.apply_history_policy(&previous, &settings) {
        rollback_shortcuts();
        return Err(command_error(error));
    }
    if let Err(error) = storage.update_settings(&settings) {
        rollback_shortcuts();
        return Err(command_error(error));
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
    limit: Option<u32>,
    storage: State<'_, Storage>,
) -> Result<Vec<HistoryItem>, String> {
    storage
        .list_history(limit.unwrap_or(100))
        .map_err(command_error)
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

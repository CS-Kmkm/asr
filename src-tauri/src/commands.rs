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

#[tauri::command]
pub(crate) fn get_app_state(state: State<'_, AppState>) -> AppStateSnapshot {
    state.snapshot()
}

#[tauri::command]
pub(crate) async fn list_audio_devices(
    services: State<'_, Services>,
) -> Result<Vec<AudioDevice>, String> {
    let audio = take_audio(&services)?;
    let result = audio.list_devices().await.map_err(command_error);
    return_audio(&services, audio);
    result
}

#[tauri::command]
pub(crate) async fn start_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    let operation_id = services.lifecycle.begin_start()?;
    let guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    let settings = storage.get_settings().map_err(command_error)?;
    ensure_model_loaded(&app, &services, &settings).await?;
    let target = SystemTextInjector::default()
        .capture_target()
        .map_err(command_error)?;
    let capture_config = capture_config(&settings);
    let mut audio = take_audio(&services)?;
    // Ensure the stream is warm (idempotent if a prior stop left it armed) so
    // its preroll ring already holds the audio captured during warm-up, then
    // promote it to recording without losing the leading edge of speech.
    let result = async {
        audio.arm(capture_config.clone()).await?;
        audio.start(capture_config).await
    }
    .await
    .map_err(command_error);
    return_audio(&services, audio);
    result?;
    if let Err(error) = services.lifecycle.mark_recording(operation_id) {
        let mut audio = take_audio(&services)?;
        let cancel_result = audio.cancel().await.map_err(command_error);
        return_audio(&services, audio);
        cancel_result?;
        return Err(error.into());
    }
    *services
        .target
        .lock()
        .map_err(|_| "target service is unavailable".to_string())? = Some(target);
    state.clear_result();
    emit_state(
        &app,
        &state,
        AppPhase::Recording,
        "Recording from the selected microphone.",
    );
    std::mem::forget(guard);

    let app_for_levels = app.clone();
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let Some(services) = app_for_levels.try_state::<Services>() else {
                break;
            };
            if services.lifecycle.phase() != PipelinePhase::Recording {
                break;
            }
            let level = services
                .audio
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(|audio| audio.level()))
                .unwrap_or_default();
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
    let (operation_id, cancel) = match services.lifecycle.begin_processing() {
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
    let settings = storage.get_settings().map_err(|error| {
        emit_state(
            &app,
            &state,
            AppPhase::Error,
            "Recording could not be completed. Start a new recording and try again.",
        );
        command_error(error)
    })?;
    let mut audio = take_audio(&services).map_err(|error| {
        emit_state(
            &app,
            &state,
            AppPhase::Error,
            "Recording could not be completed. Start a new recording and try again.",
        );
        error
    })?;
    let artifact_result = audio.stop().await;
    // Re-arm immediately so the next hotkey press records with zero warm-up
    // latency. A best-effort arm; failure simply falls back to cold start.
    let _ = audio.arm(capture_config(&settings)).await;
    return_audio(&services, audio);
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
    let mut artifact_cleanup = TempArtifact::new(
        artifact.path.clone(),
        !settings.delete_audio_after_processing,
    );
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
            emit_state(&app, &state, AppPhase::Idle, "Dictation cancelled.");
            return Err("dictation was cancelled".into());
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
        emit_state(&app, &state, AppPhase::Idle, "Dictation cancelled.");
        return Err("dictation was cancelled".into());
    }

    let target = services
        .target
        .lock()
        .map_err(|_| "target service is unavailable".to_string())?
        .take()
        .ok_or_else(|| "recording target is unavailable".to_string())?;
    let injector = SystemTextInjector::new(InjectionOptions {
        restore_clipboard: settings.clipboard_restore,
    });
    let input_monitor = Arc::clone(&services.input_monitor);
    let mut final_text = transcript.text.clone();
    let mut processed_text = None;
    let mut llm_provider = None;
    let mut correction_failed = false;
    let mut streamed_into_target = false;
    let mut streaming_session = None;
    if settings.text_correction_enabled {
        let correction_hints = storage
            .dictionary_correction_hints(&transcript.text)
            .unwrap_or_default();
        match injector.begin_streaming(&transcript.text, &target, &input_monitor) {
            Ok(session) => {
                streamed_into_target = session.is_some();
                streaming_session = session;
            }
            Err(error) => emit_status(
                &app,
                "streaming_insertion_unavailable",
                &format!("Live replacement is unavailable; waiting for final text. {error}"),
            ),
        }
        emit_correction_preview(&app, &transcript.text, "draft");
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
                if let Some(session) = streaming_session.as_mut() {
                    let _ = injector.push_stream_delta(session, delta, &input_monitor);
                }
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
                if let Some(session) = streaming_session.as_mut() {
                    injector.cancel_streaming(session, &input_monitor);
                }
                input_monitor.shutdown();
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
                    &format!("AI correction failed; using the original transcript. {error}"),
                );
            }
        }
    } else {
        emit_correction_preview(&app, &transcript.text, "final");
    }
    if services.lifecycle.is_cancelled(operation_id) {
        if let Some(session) = streaming_session.as_mut() {
            injector.cancel_streaming(session, &input_monitor);
        }
        input_monitor.shutdown();
        emit_state(&app, &state, AppPhase::Idle, "Dictation cancelled.");
        return Err("dictation was cancelled".into());
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
    let insertion_result = if let Some(session) = streaming_session.as_mut() {
        injector.finish_streaming(session, &final_text, &input_monitor)
    } else {
        injector.insert(&final_text, &target)
    };
    // The helper observes input only while a provisional replacement session
    // can still mutate the target. Stop it before persisting the result.
    input_monitor.shutdown();
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
    let insertion_label = if streamed_into_target && insertion != InsertResult::ClipboardOnly {
        "streaming_replace"
    } else {
        match insertion {
            InsertResult::UiAutomation => "ui_automation",
            InsertResult::ClipboardPaste => "clipboard_paste",
            InsertResult::UnicodeInput => "unicode_input",
            InsertResult::ClipboardOnly => "clipboard_only",
        }
    };
    storage
        .add_history(&NewHistoryItem {
            transcript_text: &transcript.text,
            processed_text: processed_text.as_deref(),
            mode: if processed_text.is_some() {
                "ai_corrected"
            } else if correction_failed {
                "faithful_fallback"
            } else {
                "faithful"
            },
            asr_provider: &transcript.model,
            llm_provider: llm_provider.as_deref(),
            app_category: None,
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
        })
        .map_err(command_error)?;
    storage
        .add_metric(
            "dictation",
            Some(&transcript.model),
            Some(latency_ms as i64),
            true,
            None,
        )
        .map_err(command_error)?;
    let completion = if insertion == InsertResult::ClipboardOnly && streamed_into_target {
        "User activity or a target change stopped live replacement; the final result remains on the clipboard."
    } else if insertion == InsertResult::ClipboardOnly {
        "Automatic insertion failed; the result remains on the clipboard."
    } else if correction_failed {
        "AI correction failed; the original transcript was inserted."
    } else {
        "Dictation inserted successfully."
    };
    recording_overlay::set_phase(&app, &AppPhase::Completed);
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

fn emit_correction_preview(app: &AppHandle, text: &str, stage: &str) {
    let _ = app.emit(
        "correction-preview",
        serde_json::json!({ "text": text, "stage": stage }),
    );
}

#[tauri::command]
pub(crate) async fn cancel_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let Some((operation_id, phase)) = services.lifecycle.cancel()? else {
        return Ok(());
    };
    if let Ok(mut target) = services.target.lock() {
        target.take();
    }
    if matches!(phase, PipelinePhase::Starting | PipelinePhase::Recording) {
        if let Ok(mut audio) = take_audio(&services) {
            let result = audio.cancel().await.map_err(command_error);
            return_audio(&services, audio);
            result?;
            services.lifecycle.finish(operation_id);
        } else if phase == PipelinePhase::Recording {
            return Err("audio operation is already in progress".into());
        }
    }
    emit_state(&app, &state, AppPhase::Idle, "Dictation cancelled.");
    Ok(())
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
    if settings.hotkey.trim().is_empty() {
        return Err("hotkey cannot be empty".into());
    }
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
        return Err("text correction provider must be openai or gemini".into());
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
    ] {
        if model.trim().is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
            return Err(format!(
                "{label} must be non-empty, at most 512 characters, and contain no control characters"
            ));
        }
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
    let previous = storage.get_settings().map_err(command_error)?;
    let old_shortcut = parse_shortcut(&previous.hotkey)?;
    if new_shortcut != old_shortcut {
        app.global_shortcut()
            .register(new_shortcut)
            .map_err(|error| format!("hotkey registration failed: {error}"))?;
        if let Err(error) = app.global_shortcut().unregister(old_shortcut) {
            let _ = app.global_shortcut().unregister(new_shortcut);
            return Err(format!("hotkey update failed: {error}"));
        }
    }
    if let Err(error) = storage.apply_history_policy(&previous, &settings) {
        if new_shortcut != old_shortcut {
            let _ = app.global_shortcut().register(old_shortcut);
            let _ = app.global_shortcut().unregister(new_shortcut);
        }
        return Err(command_error(error));
    }
    if let Err(error) = storage.update_settings(&settings) {
        if new_shortcut != old_shortcut {
            let _ = app.global_shortcut().register(old_shortcut);
            let _ = app.global_shortcut().unregister(new_shortcut);
        }
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
    emit_status(
        app,
        "model_loading",
        "Preparing the speech model. The first use may download model files.",
    );
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

    if let Err(error) = services
        .transcriber
        .load(&settings.model_quantization)
        .await
    {
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

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

const EDIT_CLIPBOARD_UNAVAILABLE: &str =
    "The original selection was not changed, and the clipboard is unavailable; the edit is available in this app.";

/// A replacement error happens before any paste, so the original selection is
/// unchanged and the clipboard is the fallback. `None` means the edit reached
/// neither the target nor the clipboard.
fn edit_insertion_outcome<E, F>(
    replaced: Result<InsertResult, E>,
    copy_to_clipboard: impl FnOnce() -> Result<(), F>,
) -> Option<InsertResult> {
    match replaced {
        Ok(result) => Some(result),
        Err(_) => copy_to_clipboard()
            .ok()
            .map(|()| InsertResult::ClipboardOnly),
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

/// This warning follows, and visually replaces, the completion notice. Only a
/// confirmed paste may be described as a completed edit.
fn edit_persistence_warning(
    insertion: InsertResult,
    history_failed: bool,
    metric_failed: bool,
) -> Option<(&'static str, &'static str)> {
    let replaced = insertion == InsertResult::ClipboardPaste;
    let warning = match (history_failed, metric_failed, replaced) {
        (false, false, _) => return None,
        (true, true, true) => (
            "history_metric_save_failed",
            "The edit was completed, but history and usage metrics could not be saved.",
        ),
        (true, false, true) => (
            "history_save_failed",
            "The edit was completed, but history could not be saved.",
        ),
        (false, true, true) => (
            "metric_save_failed",
            "The edit was completed, but usage metrics could not be saved.",
        ),
        (true, true, false) => (
            "history_metric_save_failed",
            "The edit remains on the clipboard, but history and usage metrics could not be saved.",
        ),
        (true, false, false) => (
            "history_save_failed",
            "The edit remains on the clipboard, but history could not be saved.",
        ),
        (false, true, false) => (
            "metric_save_failed",
            "The edit remains on the clipboard, but usage metrics could not be saved.",
        ),
    };
    Some(warning)
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

/// Upper bound on waiting for the start cue before capture opens. Windows
/// Plays the start cue to completion before opening the microphone. Timing
/// out an in-flight `spawn_blocking` sound would let capture start while that
/// sound is still audible, so the fixed OS cue must finish first.
async fn play_start_cue_before_capture<Cue>(enabled: bool, cue: impl FnOnce() -> Cue)
where
    Cue: std::future::Future<Output = ()>,
{
    if enabled {
        cue().await;
    }
}

async fn play_start_cue_to_completion() {
    let _ = tauri::async_runtime::spawn_blocking(|| {
        #[cfg(target_os = "windows")]
        {
            use windows::core::w;
            use windows::Win32::Foundation::HMODULE;
            use windows::Win32::Media::Audio::{
                PlaySoundW, SND_ALIAS, SND_NODEFAULT, SND_SYNC, SND_SYSTEM,
            };
            // The SystemAsterisk cue formerly sent through MessageBeep, played
            // synchronously so completion is observable.
            let _ = unsafe {
                PlaySoundW(
                    w!("SystemAsterisk"),
                    HMODULE::default(),
                    SND_ALIAS | SND_NODEFAULT | SND_SYNC | SND_SYSTEM,
                )
            };
        }
    })
    .await;
}

/// Stop and cancel cues play after capture has closed, so they need not block.
fn play_stop_cue(enabled: bool) {
    dispatch_interaction_sound(enabled, || {
        tauri::async_runtime::spawn_blocking(|| {
            #[cfg(target_os = "windows")]
            {
                use windows::Win32::System::Diagnostics::Debug::MessageBeep;
                use windows::Win32::UI::WindowsAndMessaging::MB_OK;
                let _ = unsafe { MessageBeep(MB_OK) };
            }
        });
    });
}

fn dispatch_interaction_sound(enabled: bool, dispatch: impl FnOnce()) {
    if enabled {
        dispatch();
    }
}

async fn release_microphone_test(
    test: &mut MicrophoneTestState,
    audio: &mut dyn AudioCapture,
) -> Result<bool, audio::AudioError> {
    if !test.is_active() {
        return Ok(false);
    }
    audio.disarm().await?;
    test.stop();
    Ok(true)
}

#[tauri::command]
pub(crate) async fn start_microphone_test(
    app: AppHandle,
    device_id: Option<String>,
    services: State<'_, Services>,
) -> Result<(), String> {
    let mut test = services.microphone_test.lock().await;
    if services.lifecycle.phase() != PipelinePhase::Idle {
        return Err("microphone test requires an idle recording pipeline".into());
    }
    let mut audio = services.audio.lock().await;
    if !release_microphone_test(&mut test, audio.as_mut())
        .await
        .map_err(|error| format!("Microphone test could not start. {error}"))?
    {
        audio
            .disarm()
            .await
            .map_err(|error| format!("Microphone test could not start. {error}"))?;
    }
    let config = CaptureConfig {
        device_id,
        preroll: Duration::ZERO,
        ..CaptureConfig::default()
    };
    audio
        .arm(config)
        .await
        .map_err(|error| format!("Microphone test could not start. {error}"))?;
    if services.lifecycle.phase() != PipelinePhase::Idle {
        audio
            .disarm()
            .await
            .map_err(|error| format!("Microphone test could not start. {error}"))?;
        return Err("microphone test was interrupted by recording".into());
    }
    let generation = test.start();
    drop(audio);
    drop(test);
    tauri::async_runtime::spawn(async move {
        let mut failed_stream = false;
        loop {
            tokio::time::sleep(Duration::from_millis(80)).await;
            let services = app.state::<Services>();
            let mut test = services.microphone_test.lock().await;
            if !test.is_current(generation) {
                break;
            }
            let mut audio = services.audio.lock().await;
            let first_error = if failed_stream {
                None
            } else {
                audio.stream_error()
            };
            if first_error.is_some() {
                failed_stream = true;
            }
            if failed_stream {
                let released = release_microphone_test(&mut test, audio.as_mut())
                    .await
                    .is_ok();
                drop(audio);
                drop(test);
                if let Some(error) = first_error {
                    // A stable prefix lets the frontend localize the device error.
                    emit_status(
                        &app,
                        "microphone_test_failed",
                        &format!("Microphone test failed. {error}"),
                    );
                }
                if released {
                    break;
                }
                continue;
            }
            let level = audio.level();
            drop(audio);
            drop(test);
            let _ = app.emit("audio-level", level);
        }
    });
    Ok(())
}

#[tauri::command]
pub(crate) async fn stop_microphone_test(services: State<'_, Services>) -> Result<(), String> {
    let mut test = services.microphone_test.lock().await;
    let mut audio = services.audio.lock().await;
    release_microphone_test(&mut test, audio.as_mut())
        .await
        .map_err(|error| format!("Microphone test could not stop. {error}"))?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn start_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_recording_with_origin(app, services, state, storage, false, None).await
}

#[tauri::command]
pub(crate) async fn start_voice_translation(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_voice_translation_with_origin(app, services, state, storage, false, None).await
}

#[tauri::command]
pub(crate) async fn start_speak_to_edit(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_speak_to_edit_with_origin(app, services, state, storage, false, None).await
}

#[tauri::command]
pub(crate) async fn start_ask(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    start_ask_with_origin(app, services, state, storage, false, None).await
}

pub(crate) async fn start_recording_with_origin(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
    trigger_chord: Option<String>,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        trigger_chord,
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
    trigger_chord: Option<String>,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        trigger_chord,
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
    trigger_chord: Option<String>,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        trigger_chord,
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
    trigger_chord: Option<String>,
) -> Result<(), String> {
    start_recording_mode(
        app,
        services,
        state,
        storage,
        from_shortcut,
        trigger_chord,
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
    trigger_chord: Option<String>,
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
    let active_hotkey = trigger_chord.as_deref().unwrap_or_else(|| match mode {
        PipelineMode::Dictate => &settings.shortcuts.dictate[0],
        PipelineMode::Translate => &settings.shortcuts.translate[0],
        PipelineMode::Edit => &settings.shortcuts.edit[0],
        PipelineMode::Ask => &settings.shortcuts.ask[0],
    });
    let mode_shortcuts = match mode {
        PipelineMode::Dictate => &settings.shortcuts.dictate,
        PipelineMode::Translate => &settings.shortcuts.translate,
        PipelineMode::Edit => &settings.shortcuts.edit,
        PipelineMode::Ask => &settings.shortcuts.ask,
    };
    let mut edit_session = (mode == PipelineMode::Edit)
        .then(|| EditSession::new(&settings, active_hotkey, from_shortcut))
        .transpose()?;
    let mut ask_session = (mode == PipelineMode::Ask)
        .then(|| AskSession::new(&settings, active_hotkey, from_shortcut));
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
    // The start cue finishes before the microphone opens (`arm` below), so it
    // is neither recorded nor detected as speech by the live draft.
    play_start_cue_before_capture(settings.interaction_sounds, play_start_cue_to_completion).await;
    if services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
    // Edit and Ask capture a selection instead of a target window, so their
    // ASR hints use global dictionary entries only; they apply no style profile.
    let app_context = target.as_ref().and_then(app_context::from_target);
    let mut live_slot = services.live.lock().await;
    let mut edit_slot = services.edit.lock().await;
    let mut ask_slot = services.ask.lock().await;
    let draft = target.as_ref().map(|target| {
        live_dictation::LiveDraft::new(
            target.clone(),
            &settings,
            active_hotkey,
            mode_shortcuts,
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
    let prompt = storage
        .dictionary_asr_prompt_for(app_context.as_ref(), &settings.asr_backend)
        .unwrap_or_default();
    let capture_config = capture_config(&settings);
    let mut test = services.microphone_test.lock().await;
    let mut audio = services.audio.lock().await;
    release_microphone_test(&mut test, audio.as_mut())
        .await
        .map_err(command_error)?;
    drop(test);
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
    *services
        .app_context
        .lock()
        .map_err(|_| "target service is unavailable".to_string())? = app_context;
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
    *live_slot = draft.map(|draft| {
        live_dictation::start(
            app.clone(),
            operation_id,
            draft,
            prompt,
            settings.speech_locale.clone(),
            cancel,
        )
    });
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
    play_stop_cue(
        storage
            .get_settings()
            .map(|settings| settings.interaction_sounds)
            .unwrap_or(false),
    );
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

    let app_context = services
        .app_context
        .lock()
        .map_err(|_| "target service is unavailable".to_string())?
        .clone();
    let prompt = storage
        .dictionary_asr_prompt_for(app_context.as_ref(), &settings.asr_backend)
        .unwrap_or_default();
    let correction_cancel = cancel.clone();
    let transcript_result = services
        .transcriber
        .transcribe_with_locale(
            &artifact.path,
            prompt.as_deref(),
            settings.speech_locale.as_deref(),
            cancel,
        )
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
            Err(error) => {
                let _ = storage.add_metric(
                    "speak_to_edit",
                    Some(settings.correction_provider.as_str()),
                    Some(edit_started.elapsed().as_millis() as i64),
                    false,
                    Some("edit_failed"),
                );
                emit_state(&app, &state, AppPhase::Error, edit_failure_message(&error));
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
        let insertion = edit_insertion_outcome(
            session.injector.replace_selection(
                &session.selection,
                &edited,
                &session.monitor,
                session.checkpoint,
            ),
            || session.injector.copy_to_clipboard(&edited),
        );
        recording_overlay::set_phase(&app, &AppPhase::Completed);
        let Some(insertion) = insertion else {
            // Neither the target nor the clipboard received the edit. Keep it
            // recoverable in this app and end the operation visibly.
            state.publish_result(edited.clone());
            emit_state(&app, &state, AppPhase::Error, EDIT_CLIPBOARD_UNAVAILABLE);
            return Err(EDIT_CLIPBOARD_UNAVAILABLE.into());
        };
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
        if result.is_ok() {
            recording_overlay::set_phase(&app, &AppPhase::Completed);
        }
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
    let mut correction_output_limited = false;
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
        let Some(target_language) = translation_target.as_deref() else {
            // Startup publishes the target with the session. If it is missing,
            // keep the speech the same way a translation failure does.
            let copied = draft.injector.copy_to_clipboard(&transcript.text).is_ok();
            emit_correction_preview(&app, &transcript.text, "fallback");
            emit_state(
                &app,
                &state,
                AppPhase::Error,
                if copied {
                    "The translation target language was unavailable; the raw transcript remains on the clipboard."
                } else {
                    "The translation target language was unavailable and the clipboard could not be updated; the raw transcript remains available in this app."
                },
            );
            return Err("translation target language is unavailable".into());
        };
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
                // Publish before insertion so an insertion error cannot lose
                // the completed translation.
                let _ = app.emit("app-state", state.publish_result(final_text.clone()));
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
            .dictionary_correction_hints(&transcript.text, app_context.as_ref())
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
            personalization::resolve_profile(&settings, app_context.as_ref())
                .map(|profile| personalization::guidance(&profile))
                .as_deref(),
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
                correction_output_limited =
                    matches!(error, correction::CorrectionError::OutputLimit);
                emit_correction_preview(&app, &transcript.text, "fallback");
                let _ = storage.add_metric(
                    "text_correction",
                    Some(settings.correction_provider.as_str()),
                    Some(correction_started.elapsed().as_millis() as i64),
                    false,
                    Some(correction_failure_metric_code(&error)),
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
            app_category: app_context::history_category(app_context.as_ref()),
            duration_ms: Some(duration_ms as i64),
            latency_ms: Some(latency_ms as i64),
            retry_of_id: None,
        },
        Some(&artifact.path),
    );
    // Only an AI correction of a Dictate transcript proposes a spelling; a
    // translation is a different text, not a correction of the transcript.
    if mode == PipelineMode::Dictate && processed_text.is_some() {
        let _ =
            storage.add_dictionary_candidate_from_correction(&transcript.text, &final_text, None);
    }
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
    } else if correction_output_limited {
        "AI correction stopped at the local output token limit; the original transcript was inserted. Increase the local output token limit."
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
    require_ask_spoken_input(spoken)?;
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
                app_category: None,
                duration_ms: Some(duration_ms as i64),
                latency_ms: Some(latency_ms as i64),
                retry_of_id: None,
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
            let result =
                session
                    .monitor
                    .as_ref()
                    .zip(session.checkpoint)
                    .map(|(monitor, checkpoint)| {
                        session.injector.replace_selection_monitored(
                            selection, &output, monitor, checkpoint, &cancel,
                        )
                    });
            if *cancel.borrow() {
                return Err("ask was cancelled".into());
            }
            match result {
                Some(Ok(InsertResult::ClipboardPaste)) => insertion = "selection",
                Some(Ok(InsertResult::ClipboardOnly)) => {
                    if *cancel.borrow() {
                        return Err("ask was cancelled".into());
                    }
                    insertion = "clipboard_only";
                }
                Some(Ok(InsertResult::PasteUnverified)) => insertion = "paste_unverified",
                Some(Err(injection::InjectionError::Cancelled)) => {
                    return Err("ask was cancelled".into())
                }
                _ if !*cancel.borrow() => {
                    session
                        .injector
                        .copy_to_clipboard(&output)
                        .map_err(command_error)?;
                    insertion = "clipboard_only";
                }
                _ => return Err("ask was cancelled".into()),
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
                    if *cancel.borrow() {
                        return Err("ask was cancelled".into());
                    }
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

fn require_ask_spoken_input(spoken: &str) -> Result<(), String> {
    if spoken.trim().is_empty() {
        Err("No Ask instruction was captured.".into())
    } else {
        Ok(())
    }
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
/// cannot observe the old settings after the purge. Shortcut registrations are
/// rolled back by `shortcuts::update_registrations` when this fails.
fn persist_settings(storage: &Storage, settings: &Settings) -> Result<(), String> {
    storage
        .update_settings_and_apply_history_policy(settings)
        .map_err(command_error)
}

/// Names each inactive saved shortcut; the frontend localizes the fixed text
/// and the action names.
fn startup_shortcut_warning(inactive: &[String]) -> Option<String> {
    (!inactive.is_empty()).then(|| {
        format!(
            "Some saved hotkeys overlap or could not be registered. Change them in Settings. Inactive: {}",
            inactive.join(", ")
        )
    })
}

#[tauri::command]
pub(crate) fn get_startup_hotkey_warning(services: State<'_, Services>) -> Option<String> {
    startup_shortcut_warning(
        &services
            .inactive_shortcuts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

fn emit_correction_preview(app: &AppHandle, text: &str, stage: &str) {
    let _ = app.emit(
        "correction-preview",
        serde_json::json!({ "text": text, "stage": stage }),
    );
}

fn next_translation_target(languages: &[String], current: &str) -> Option<String> {
    let current_index = languages
        .iter()
        .position(|language| language == current)
        .unwrap_or(0);
    languages
        .get((current_index + 1) % languages.len().max(1))
        .cloned()
}

/// Persists the target after `active` (the recording's target, falling back
/// to the saved one). Holding the settings-write lock keeps this
/// read-modify-write from reverting a concurrent `update_settings`.
fn store_next_translation_target(
    storage: &Storage,
    active: Option<&str>,
) -> Result<Settings, String> {
    let _settings_writes = storage.lock_settings_writes().map_err(command_error)?;
    let mut settings = storage.get_settings().map_err(command_error)?;
    let current = active.unwrap_or(settings.translation_target_language.as_str());
    settings.translation_target_language =
        next_translation_target(&settings.translation_target_languages, current)
            .ok_or("at least one translation target language is required")?;
    storage.update_settings(&settings).map_err(command_error)?;
    Ok(settings)
}

#[tauri::command]
pub(crate) async fn cycle_voice_translation_target(
    app: AppHandle,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<String, String> {
    let _update_guard = services.settings_update.lock().await;
    // An async command runs off the main thread. Settings saves can hold the
    // settings-write lock while the global-shortcut plugin waits on that
    // thread; waiting for the lock here must not block shortcut registration.
    let mut active = services
        .voice_translation_target
        .lock()
        .map_err(|_| "translation target service is unavailable".to_string())?;
    if services.lifecycle.phase() != PipelinePhase::Recording
        || services.lifecycle.mode() != Some(PipelineMode::Translate)
    {
        return Err("voice translation is not recording".into());
    }
    let settings = store_next_translation_target(&storage, active.as_deref())?;
    let next = settings.translation_target_language.clone();
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
    storage: State<'_, Storage>,
) -> Result<(), String> {
    let mode = services.lifecycle.mode().unwrap_or(PipelineMode::Dictate);
    let cancelled = cancel_pipeline_operation(
        &services.lifecycle,
        &services.audio,
        &services.target,
        &services.voice_translation_target,
        &services.live,
        &services.edit,
        &services.ask,
    )
    .await?;
    if cancelled.is_none() {
        return Ok(());
    }
    recording_overlay::set_interactive(&app, false);
    if cancelled == Some(PipelinePhase::Recording) {
        play_stop_cue(
            storage
                .get_settings()
                .map(|settings| settings.interaction_sounds)
                .unwrap_or(false),
        );
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
    translation_target: &Mutex<Option<String>>,
    live: &tokio::sync::Mutex<Option<live_dictation::LiveTask>>,
    edit: &tokio::sync::Mutex<Option<EditSession>>,
    ask: &tokio::sync::Mutex<Option<AskSession>>,
) -> Result<Option<PipelinePhase>, String> {
    let Some((operation_id, phase)) = lifecycle.cancel()? else {
        return Ok(None);
    };
    // Starting and Processing already have an owner. Releasing their lifecycle
    // here would allow a new recording to race their remaining writes/cleanup.
    if phase != PipelinePhase::Recording {
        return Ok(Some(phase));
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
    // Startup publishes the target language before releasing `live`. Clear it
    // while this operation still owns the lifecycle, so a later recording's
    // target is never removed.
    if let Ok(mut language) = translation_target.lock() {
        language.take();
    }
    let audio_result = {
        let mut audio = audio.lock().await;
        audio.cancel().await
    };
    if let Some(task) = live_task {
        drop(task.finish().await?);
    }
    audio_result.map_err(command_error)?;
    Ok(Some(phase))
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
        correction::CorrectionError::OutputLimit => {
            return "AI correction stopped at the local output token limit; using the original transcript. Increase the local output token limit.".into();
        }
        correction::CorrectionError::ProtectedContentChanged => "protected_content_changed",
        correction::CorrectionError::Cancelled => "cancelled",
        correction::CorrectionError::InvalidEndpoint(_) => "invalid_endpoint",
        correction::CorrectionError::UnsupportedProvider(_) => "unsupported_provider",
        correction::CorrectionError::EmptyEditInstruction => "empty_edit_instruction",
    };
    format!("AI correction failed; using the original transcript. Error kind: {kind}.")
}

/// Names why Speak to edit failed, so the user can fix the cause. Every
/// message is a fixed, localized string; error details are never shown.
fn edit_failure_message(error: &correction::CorrectionError) -> &'static str {
    use correction::CorrectionError;
    match error {
        CorrectionError::MissingApiKey(_) => {
            "Editing failed because the AI provider API key is not set. The original selection was not changed."
        }
        CorrectionError::Api { status, .. } if matches!(status.as_u16(), 401 | 403) => {
            "Editing failed because the AI provider rejected the API key. The original selection was not changed."
        }
        CorrectionError::Api { status, .. } if status.as_u16() == 429 => {
            "Editing failed because the AI provider rate limit was reached. Try again later. The original selection was not changed."
        }
        CorrectionError::Api { .. } => {
            "Editing failed because the AI provider returned an error. The original selection was not changed."
        }
        CorrectionError::Request(_) => {
            "Editing failed because the AI provider could not be reached. The original selection was not changed."
        }
        CorrectionError::InvalidEndpoint(_) => {
            "Editing failed because the local AI endpoint URL is invalid. The original selection was not changed."
        }
        CorrectionError::OutputLimit => {
            "Editing stopped at the local output token limit. The original selection was not changed. Increase the local output token limit."
        }
        CorrectionError::InvalidResponse(_) | CorrectionError::ProtectedContentChanged => {
            "Editing failed because the AI provider returned an unusable response. The original selection was not changed."
        }
        CorrectionError::UnsupportedProvider(_) => {
            "Editing failed because the selected AI provider is not supported. The original selection was not changed."
        }
        CorrectionError::EmptyEditInstruction => {
            "No edit instruction was captured; the original selection was not changed."
        }
        CorrectionError::Cancelled => "Editing cancelled.",
    }
}

/// Metric code for a failed correction; validator rejects stay countable.
fn correction_failure_metric_code(error: &correction::CorrectionError) -> &'static str {
    match error {
        correction::CorrectionError::ProtectedContentChanged => "protected_content_changed",
        _ => "correction_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_ask_input_is_rejected_before_planning() {
        for spoken in ["", "  ", "\n\t"] {
            assert_eq!(
                require_ask_spoken_input(spoken),
                Err("No Ask instruction was captured.".to_string())
            );
        }
        assert!(require_ask_spoken_input("search Google for Rust").is_ok());
    }
    use crate::audio::{AudioArtifact, AudioError, AudioFuture, CaptureState, LevelMeter};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn retry_dictate_label_tracks_correction_even_when_text_is_unchanged() {
        assert_eq!(
            retry_dictate_history_mode(Some("local"), false),
            "ai_corrected"
        );
        assert_eq!(retry_dictate_history_mode(None, false), "faithful");
        assert_eq!(retry_dictate_history_mode(None, true), "faithful_fallback");
    }

    #[test]
    fn load_stages_announce_download_resume_and_verification() {
        let progress = |stage: &str, resumed_bytes| asr::LoadProgress {
            stage: stage.into(),
            model: Some("repo".into()),
            completed_bytes: None,
            total_bytes: None,
            resumed_bytes,
        };
        assert_eq!(
            load_stage_notice(&progress("download", Some(4096))).map(|(_, message)| message),
            Some("Resuming the interrupted speech model download.")
        );
        assert_eq!(
            load_stage_notice(&progress("download", None)).map(|(kind, _)| kind),
            Some("model_downloading")
        );
        assert_eq!(
            load_stage_notice(&progress("verify", None)).map(|(kind, _)| kind),
            Some("model_verifying")
        );
        assert_eq!(load_stage_notice(&progress("load", None)), None);
    }

    #[test]
    fn edit_failures_name_their_cause() {
        use correction::CorrectionError;
        use reqwest::StatusCode;
        let api = |status| CorrectionError::Api {
            status,
            message: "details".into(),
        };
        let messages = [
            edit_failure_message(&CorrectionError::MissingApiKey("OPENAI_API_KEY".into())),
            edit_failure_message(&api(StatusCode::UNAUTHORIZED)),
            edit_failure_message(&api(StatusCode::TOO_MANY_REQUESTS)),
            edit_failure_message(&api(StatusCode::INTERNAL_SERVER_ERROR)),
            edit_failure_message(&CorrectionError::InvalidEndpoint("ftp://".into())),
            edit_failure_message(&CorrectionError::OutputLimit),
            edit_failure_message(&CorrectionError::InvalidResponse("empty".into())),
            edit_failure_message(&CorrectionError::UnsupportedProvider("other".into())),
            edit_failure_message(&CorrectionError::EmptyEditInstruction),
        ];
        let distinct = messages.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(distinct.len(), messages.len());
        assert_eq!(
            edit_failure_message(&api(StatusCode::FORBIDDEN)),
            edit_failure_message(&api(StatusCode::UNAUTHORIZED))
        );
        // Provider details may echo request content, so they are never shown.
        assert!(messages.iter().all(|message| !message.contains("details")));
    }

    #[test]
    fn retry_keeps_the_transcript_when_correction_fails_or_is_rejected() {
        assert!(matches!(
            classify_retry_correction(Ok("corrected".into())),
            RetryCorrection::Corrected(text) if text == "corrected"
        ));
        assert!(matches!(
            classify_retry_correction(Err(correction::CorrectionError::Cancelled)),
            RetryCorrection::Cancelled
        ));
        for error in [
            correction::CorrectionError::ProtectedContentChanged,
            correction::CorrectionError::OutputLimit,
            correction::CorrectionError::MissingApiKey("OPENAI_API_KEY".into()),
            correction::CorrectionError::InvalidResponse("empty".into()),
        ] {
            assert!(matches!(
                classify_retry_correction(Err(error)),
                RetryCorrection::Fallback(_)
            ));
        }
    }

    #[test]
    fn retry_routes_personalization_by_the_saved_category_only() {
        let item = |app_category: Option<&str>| HistoryItem {
            id: 1,
            transcript_text: "text".into(),
            processed_text: None,
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: "faithful".into(),
            asr_provider: "mock".into(),
            llm_provider: None,
            target_language: None,
            app_category: app_category.map(str::to_owned),
            duration_ms: None,
            latency_ms: None,
            created_at: String::new(),
            has_audio: true,
            retry_of_id: None,
        };
        let profile = |formality: &str| types::StyleProfile {
            formality: formality.into(),
            detail: "concise".into(),
            guidance: None,
        };
        let settings = Settings {
            personalization_enabled: true,
            global_style_profile: Some(profile("formal")),
            scoped_style_profiles: vec![
                types::ScopedStyleProfile {
                    scope: "category:messaging".into(),
                    profile: profile("casual"),
                },
                types::ScopedStyleProfile {
                    scope: "app:slack".into(),
                    profile: profile("formal"),
                },
            ],
            ..Settings::default()
        };

        let messaging = retry_app_context(&item(Some("messaging")));
        assert_eq!(
            messaging,
            Some(types::AppContext {
                app_key: None,
                category: "messaging".into(),
            })
        );
        assert_eq!(
            personalization::resolve_profile(&settings, messaging.as_ref())
                .unwrap()
                .formality,
            "casual"
        );
        let unknown = retry_app_context(&item(None));
        assert_eq!(unknown, None);
        assert_eq!(
            personalization::resolve_profile(&settings, unknown.as_ref())
                .unwrap()
                .formality,
            "formal"
        );
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
        persist_settings(&storage, &settings).unwrap();

        assert_eq!(
            storage.get_settings().unwrap().history_retention,
            types::HistoryRetention::Never
        );
        assert_eq!(stored_history_rows(&database), 0);
    }

    #[test]
    fn selected_text_and_voice_shortcuts_swap_in_one_persisted_update() {
        let storage = Storage::in_memory().unwrap();
        let previous = storage.get_settings().unwrap();
        let old_routes = shortcuts::Routes::parse(&previous).unwrap();
        let mut settings = previous.clone();
        settings.translation_hotkey = previous.shortcuts.translate[0].clone();
        assert!(shortcuts::desired_routes_for_update(&previous, &settings, &old_routes).is_err());
        settings.shortcuts.translate[0] = previous.translation_hotkey.clone();
        let desired =
            shortcuts::desired_routes_for_update(&previous, &settings, &old_routes).unwrap();
        shortcuts::update_registrations(
            &old_routes,
            &desired,
            |_| -> Result<(), &str> { panic!("swapping existing chords needs no registration") },
            |_| -> Result<(), &str> { panic!("swapping existing chords needs no removal") },
            || persist_settings(&storage, &settings),
        )
        .unwrap();
        let saved = storage.get_settings().unwrap();
        assert_eq!(saved.translation_hotkey, previous.shortcuts.translate[0]);
        assert_eq!(saved.shortcuts.translate[0], previous.translation_hotkey);
        let routes = shortcuts::Routes::parse(&saved).unwrap();
        assert_eq!(
            routes
                .find(crate::parse_shortcut(&saved.translation_hotkey).unwrap())
                .unwrap()
                .action,
            shortcuts::Action::SelectedText
        );
        assert_eq!(
            routes
                .find(crate::parse_shortcut(&saved.shortcuts.translate[0]).unwrap())
                .unwrap()
                .action,
            shortcuts::Action::Translate
        );
    }

    #[test]
    fn failed_update_settings_write_keeps_history_and_settings() {
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
        assert!(persist_settings(&storage, &settings).is_err());

        // The purge shares the failed settings transaction, so History-off
        // cannot erase rows while the stored settings stay unchanged. The
        // shortcut rollback on this error is covered by
        // `shortcuts::tests::persistence_failure_rolls_back_registrations`.
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
    fn edit_replacement_failure_falls_back_to_clipboard_or_reports_no_outcome() {
        for result in [
            InsertResult::ClipboardPaste,
            InsertResult::ClipboardOnly,
            InsertResult::PasteUnverified,
        ] {
            let outcome = edit_insertion_outcome(Ok::<_, ()>(result), || -> Result<(), ()> {
                panic!("a completed replacement must not copy again")
            });
            assert_eq!(outcome, Some(result));
        }
        assert_eq!(
            edit_insertion_outcome(Err::<InsertResult, _>("replace"), || Ok::<_, ()>(())),
            Some(InsertResult::ClipboardOnly)
        );
        assert_eq!(
            edit_insertion_outcome(Err::<InsertResult, _>("replace"), || Err("clipboard")),
            None
        );
    }

    #[test]
    fn edit_persistence_warning_claims_completion_only_after_a_confirmed_paste() {
        for insertion in [
            InsertResult::ClipboardPaste,
            InsertResult::ClipboardOnly,
            InsertResult::PasteUnverified,
        ] {
            assert_eq!(edit_persistence_warning(insertion, false, false), None);
            for (history_failed, metric_failed, kind) in [
                (true, true, "history_metric_save_failed"),
                (true, false, "history_save_failed"),
                (false, true, "metric_save_failed"),
            ] {
                let (actual_kind, message) =
                    edit_persistence_warning(insertion, history_failed, metric_failed).unwrap();
                assert_eq!(actual_kind, kind);
                if insertion == InsertResult::ClipboardPaste {
                    assert!(message.starts_with("The edit was completed"), "{message}");
                } else {
                    assert!(!message.contains("completed"), "{message}");
                    assert!(message.contains("remains on the clipboard"), "{message}");
                }
                assert_eq!(message.contains("history"), history_failed, "{message}");
                assert_eq!(
                    message.contains("usage metrics"),
                    metric_failed,
                    "{message}"
                );
            }
        }
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

    #[test]
    fn next_translation_target_wraps_and_falls_back_to_the_first_entry() {
        let languages = vec!["en".to_string(), "ja".to_string(), "de".to_string()];
        assert_eq!(
            next_translation_target(&languages, "ja").as_deref(),
            Some("de")
        );
        assert_eq!(
            next_translation_target(&languages, "de").as_deref(),
            Some("en")
        );
        assert_eq!(
            next_translation_target(&languages, "xx").as_deref(),
            Some("ja")
        );
        assert_eq!(next_translation_target(&[], "en"), None);
    }

    #[test]
    fn target_cycling_waits_for_an_in_progress_settings_save() {
        let storage = Arc::new(Storage::in_memory().unwrap());
        let settings_save = storage.lock_settings_writes().unwrap();
        let cycling = {
            let storage = Arc::clone(&storage);
            std::thread::spawn(move || store_next_translation_target(&storage, Some("en")))
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(!cycling.is_finished());

        // The save was based on settings read before cycling started.
        let mut saved = storage.get_settings().unwrap();
        saved.history_retention = types::HistoryRetention::Never;
        storage.update_settings(&saved).unwrap();
        drop(settings_save);

        let cycled = cycling.join().unwrap().unwrap();
        assert_eq!(cycled.translation_target_language, "ja");
        let stored = storage.get_settings().unwrap();
        assert_eq!(stored.history_retention, types::HistoryRetention::Never);
        assert_eq!(stored.translation_target_language, "ja");
    }

    struct TestAudio {
        cancel_error: bool,
        disarm_error: bool,
        disarms: Arc<AtomicUsize>,
        stream_error: Option<String>,
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
            self.disarms.fetch_add(1, Ordering::SeqCst);
            let error = self.disarm_error;
            Box::pin(async move {
                if error {
                    Err(AudioError::StreamFailure("disarm failed".into()))
                } else {
                    Ok(())
                }
            })
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

        fn stream_error(&self) -> Option<String> {
            self.stream_error.clone()
        }
    }

    fn test_audio(cancel_error: bool) -> tokio::sync::Mutex<Box<dyn AudioCapture>> {
        tokio::sync::Mutex::new(Box::new(TestAudio {
            cancel_error,
            disarm_error: false,
            disarms: Arc::new(AtomicUsize::new(0)),
            stream_error: None,
        }))
    }

    #[tokio::test]
    async fn microphone_test_release_covers_stop_error_and_recording_start() {
        for from_error in [false, true] {
            let disarms = Arc::new(AtomicUsize::new(0));
            let mut audio = TestAudio {
                cancel_error: false,
                disarm_error: false,
                disarms: Arc::clone(&disarms),
                stream_error: from_error.then(|| "device disconnected".into()),
            };
            let mut test = MicrophoneTestState::default();
            let generation = test.start();
            if from_error {
                assert_eq!(audio.stream_error().as_deref(), Some("device disconnected"));
            }
            // stop_microphone_test, recording start, and stream-error cleanup
            // all use this release path before another capture can start.
            assert!(release_microphone_test(&mut test, &mut audio)
                .await
                .unwrap());
            assert!(!test.is_current(generation));
            assert!(!release_microphone_test(&mut test, &mut audio)
                .await
                .unwrap());
            assert_eq!(disarms.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn failed_microphone_release_keeps_ownership_for_retry() {
        let disarms = Arc::new(AtomicUsize::new(0));
        let mut audio = TestAudio {
            cancel_error: false,
            disarm_error: true,
            disarms: Arc::clone(&disarms),
            stream_error: None,
        };
        let mut test = MicrophoneTestState::default();
        let generation = test.start();
        assert!(release_microphone_test(&mut test, &mut audio)
            .await
            .is_err());
        assert!(test.is_current(generation));
        audio.disarm_error = false;
        assert!(release_microphone_test(&mut test, &mut audio)
            .await
            .unwrap());
        assert!(!test.is_current(generation));
        assert_eq!(disarms.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn interaction_sound_gate_dispatches_only_enabled_cues() {
        let mut cues = 0;
        dispatch_interaction_sound(false, || cues += 1);
        dispatch_interaction_sound(true, || cues += 1);
        assert_eq!(cues, 1);
    }

    #[tokio::test]
    async fn start_cue_finishes_before_capture_opens() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let cue_events = Arc::clone(&events);
        play_start_cue_before_capture(true, move || async move {
            cue_events.lock().unwrap().push("cue started");
            tokio::time::sleep(Duration::from_millis(30)).await;
            cue_events.lock().unwrap().push("cue finished");
        })
        .await;
        // start_recording_mode arms the microphone only after the helper returns.
        events.lock().unwrap().push("capture armed");
        assert_eq!(
            *events.lock().unwrap(),
            ["cue started", "cue finished", "capture armed"]
        );
    }

    #[tokio::test]
    async fn disabled_start_cue_is_not_played_and_does_not_delay_capture() {
        let played = Arc::new(AtomicUsize::new(0));
        let cue_played = Arc::clone(&played);
        let started = Instant::now();
        play_start_cue_before_capture(false, move || async move {
            cue_played.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(5)).await;
        })
        .await;
        assert_eq!(played.load(Ordering::SeqCst), 0);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn startup_shortcut_warning_names_inactive_actions() {
        assert_eq!(startup_shortcut_warning(&[]), None);
        assert_eq!(
            startup_shortcut_warning(&[
                "Voice Translate (Ctrl+Shift+Y)".into(),
                "Ask Anything (Ctrl+Shift+A)".into(),
            ])
            .unwrap(),
            "Some saved hotkeys overlap or could not be registered. Change them in Settings. Inactive: Voice Translate (Ctrl+Shift+Y), Ask Anything (Ctrl+Shift+A)"
        );
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
        let language = Mutex::new(None);
        let cancellation =
            cancel_pipeline_operation(&lifecycle, &audio, &target, &language, &live, &edit, &ask);
        tokio::pin!(cancellation);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), cancellation.as_mut())
                .await
                .is_err()
        );
        assert!(cancel_pipeline_operation(
            &lifecycle, &audio, &target, &language, &live, &edit, &ask
        )
        .await
        .unwrap()
        .is_none());
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        drop(audio_operation);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), cancellation)
                .await
                .unwrap(),
            Ok(Some(PipelinePhase::Recording))
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
            &Mutex::new(None),
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
        let result = cancel_pipeline_operation(
            &lifecycle,
            &test_audio(false),
            &Mutex::new(None),
            &Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
        )
        .await
        .unwrap();
        assert_eq!(result, Some(PipelinePhase::Processing));
        assert!(*cancel.borrow());
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Translate).is_ok());
    }

    #[tokio::test]
    async fn cancel_clears_the_translation_target_only_when_it_cancels_a_recording() {
        let lifecycle = PipelineLifecycle::default();
        let audio = test_audio(false);
        let target = Mutex::new(None);
        let live = tokio::sync::Mutex::new(None);
        let edit = tokio::sync::Mutex::new(None);
        let ask = tokio::sync::Mutex::new(None);
        // Nothing to cancel: a target published by a concurrent start stays.
        let language = Mutex::new(Some("ja".to_string()));
        assert!(cancel_pipeline_operation(
            &lifecycle, &audio, &target, &language, &live, &edit, &ask
        )
        .await
        .unwrap()
        .is_none());
        assert_eq!(language.lock().unwrap().as_deref(), Some("ja"));

        // Processing owns its published target; cancellation only signals it.
        let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        lifecycle.begin_processing().unwrap();
        assert_eq!(
            cancel_pipeline_operation(&lifecycle, &audio, &target, &language, &live, &edit, &ask)
                .await
                .unwrap(),
            Some(PipelinePhase::Processing)
        );
        assert_eq!(language.lock().unwrap().as_deref(), Some("ja"));
        lifecycle.finish(id);

        let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        assert_eq!(
            cancel_pipeline_operation(&lifecycle, &audio, &target, &language, &live, &edit, &ask)
                .await
                .unwrap(),
            Some(PipelinePhase::Recording)
        );
        assert_eq!(*language.lock().unwrap(), None);
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

    #[test]
    fn correction_failure_status_distinguishes_the_output_limit() {
        assert_eq!(
            correction_failure_status(&correction::CorrectionError::OutputLimit),
            "AI correction stopped at the local output token limit; using the original transcript. Increase the local output token limit."
        );
        assert_eq!(
            correction_failure_status(&correction::CorrectionError::InvalidResponse(
                "missing output text".into()
            )),
            "AI correction failed; using the original transcript. Error kind: invalid_response."
        );
    }

    #[test]
    fn protected_content_rejection_has_distinct_status_and_metric() {
        let rejected = correction::CorrectionError::ProtectedContentChanged;
        assert_eq!(
            correction_failure_status(&rejected),
            "AI correction failed; using the original transcript. Error kind: protected_content_changed."
        );
        assert_eq!(
            correction_failure_metric_code(&rejected),
            "protected_content_changed"
        );

        let invalid = correction::CorrectionError::InvalidResponse("missing output text".into());
        assert!(correction_failure_status(&invalid).ends_with("Error kind: invalid_response."));
        assert_eq!(
            correction_failure_metric_code(&invalid),
            "correction_failed"
        );
    }
}

#[tauri::command]
pub(crate) fn get_settings(storage: State<'_, Storage>) -> Result<Settings, String> {
    storage.get_settings().map_err(command_error)
}

#[tauri::command]
pub(crate) fn get_shortcut_warning(services: State<'_, Services>) -> Vec<String> {
    services
        .inactive_shortcuts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[tauri::command]
pub(crate) async fn update_settings(
    app: AppHandle,
    settings: Settings,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<Settings, String> {
    let _update_guard = services.settings_update.lock().await;
    personalization::validate_settings_profiles(&settings).map_err(str::to_owned)?;
    if !["en", "ja"].contains(&settings.ui_language.as_str()) {
        return Err("ui language must be en or ja".into());
    }
    if !["system", "light", "dark"].contains(&settings.theme.as_str()) {
        return Err("theme must be system, light, or dark".into());
    }
    if settings
        .speech_locale
        .as_deref()
        .is_some_and(|locale| !types::SPEECH_LOCALES.contains(&locale))
    {
        return Err("speech locale is unsupported".into());
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
    if !types::CORRECTION_MODES.contains(&settings.correction_mode.as_str()) {
        return Err("correction mode must be conservative or intent_aware".into());
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
    // The command is async, so waiting here never blocks the main thread
    // needed by global-shortcut registration. Keep this guard through the
    // single transactional settings/History persistence step.
    let previous = {
        let _settings_writes = storage.lock_settings_writes().map_err(command_error)?;
        let previous = storage.get_settings().map_err(command_error)?;
        let old_routes = services
            .shortcut_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let desired_routes =
            shortcuts::desired_routes_for_update(&previous, &settings, &old_routes)?;
        if settings.translation_instruction.chars().count() > 500
            || settings.translation_instruction.chars().any(|character| {
                character.is_control()
                    && character != '\n'
                    && character != '\r'
                    && character != '\t'
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
        shortcuts::update_registrations(
            &old_routes,
            &desired_routes,
            |chord| app.global_shortcut().register(chord),
            |chord| app.global_shortcut().unregister(chord),
            || persist_settings(&storage, &settings),
        )?;
        let inactive = shortcuts::inactive_descriptions(&settings, &desired_routes)?;
        *services
            .shortcut_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = desired_routes;
        *services
            .inactive_shortcuts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = inactive;
        previous
    };
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
    let _ = app.emit("settings-changed", &settings);
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
        // Retry uses the current ASR settings, so it loads the selected model
        // first, as a new recording does; a backend or model change resets it.
        ensure_model_loaded(&app, &services, &settings).await?;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        // History keeps only the captured category, so Retry routes to
        // category-scoped and global dictionary entries and profiles.
        let retry_context = retry_app_context(&source);
        let prompt = storage
            .dictionary_asr_prompt_for(retry_context.as_ref(), &settings.asr_backend)
            .unwrap_or_default();
        let transcript = services
            .transcriber
            .transcribe_with_locale(
                &retry_path,
                prompt.as_deref(),
                settings.speech_locale.as_deref(),
                cancel.clone(),
            )
            .await
            .map_err(command_error)?;
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let mut output = transcript.text.clone();
        let mut llm_provider = None;
        let mut correction_failed = false;
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
                .dictionary_correction_hints(&transcript.text, retry_context.as_ref())
                .unwrap_or_default();
            let correction_started = Instant::now();
            let result = correction::correct_transcript(
                &settings,
                &transcript.text,
                &hints,
                personalization::resolve_profile(&settings, retry_context.as_ref())
                    .map(|profile| personalization::guidance(&profile))
                    .as_deref(),
                cancel.clone(),
                |_| {},
            )
            .await;
            match classify_retry_correction(result) {
                RetryCorrection::Corrected(corrected) => {
                    output = corrected;
                    llm_provider = Some(settings.correction_provider.as_str());
                }
                RetryCorrection::Cancelled => return Err("history retry was cancelled".into()),
                RetryCorrection::Fallback(error) => {
                    // Like a new recording, a failed correction keeps the new
                    // transcript instead of failing the whole Retry.
                    correction_failed = true;
                    emit_status(
                        &app,
                        "text_correction_failed",
                        &correction_failure_status(&error),
                    );
                    let _ = storage.add_metric(
                        "text_correction",
                        Some(settings.correction_provider.as_str()),
                        Some(correction_started.elapsed().as_millis() as i64),
                        false,
                        Some(correction_failure_metric_code(&error)),
                    );
                }
            }
        }
        if services.lifecycle.is_cancelled(operation_id) {
            return Err("history retry was cancelled".into());
        }
        let latency_ms = started.elapsed().as_millis() as i64;
        let history_save_status = services
            .lifecycle
            .commit_side_effect(operation_id, || {
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
                            retry_dictate_history_mode(llm_provider, correction_failed)
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
        // The committed Retry still owns the lifecycle. Publish its completion
        // before releasing it, so a recording started afterwards cannot have
        // its state overwritten by this completion.
        recording_overlay::set_phase(&app, &AppPhase::Completed);
        let snapshot = state.complete(
            output.clone(),
            "History retry completed without inserting text.".into(),
        );
        let _ = app.emit("app-state", snapshot);
        services.lifecycle.finish(operation_id);
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

fn retry_dictate_history_mode(llm_provider: Option<&str>, correction_failed: bool) -> &'static str {
    if llm_provider.is_some() {
        "ai_corrected"
    } else if correction_failed {
        "faithful_fallback"
    } else {
        "faithful"
    }
}

/// How a Retry continues after its AI correction finishes.
#[derive(Debug)]
enum RetryCorrection {
    Corrected(String),
    /// The correction failed or was rejected; the new transcript is kept.
    Fallback(correction::CorrectionError),
    Cancelled,
}

fn classify_retry_correction(
    result: Result<String, correction::CorrectionError>,
) -> RetryCorrection {
    match result {
        Ok(corrected) => RetryCorrection::Corrected(corrected),
        Err(correction::CorrectionError::Cancelled) => RetryCorrection::Cancelled,
        Err(error) => RetryCorrection::Fallback(error),
    }
}

fn retry_app_context(item: &HistoryItem) -> Option<types::AppContext> {
    item.app_category.clone().map(|category| types::AppContext {
        app_key: None,
        category,
    })
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
pub(crate) fn list_dictionary(
    query: Option<String>,
    source: Option<String>,
    storage: State<'_, Storage>,
) -> Result<Vec<DictionaryEntry>, String> {
    storage
        .search_dictionary(query.as_deref(), source.as_deref())
        .map_err(command_error)
}

#[tauri::command]
pub(crate) fn update_dictionary_entry(
    id: i64,
    entry: DictionaryEntryInput,
    storage: State<'_, Storage>,
) -> Result<DictionaryEntry, String> {
    personalization::validate_dictionary_scope(entry.app_scope.as_deref())
        .map_err(str::to_owned)?;
    if !storage
        .update_dictionary_entry(
            id,
            &NewDictionaryEntry {
                reading: &entry.reading,
                surface: &entry.surface,
                category: entry.category.as_deref(),
                aliases: &entry.aliases,
                priority: entry.priority,
                app_scope: entry.app_scope.as_deref(),
            },
        )
        .map_err(command_error)?
    {
        return Err("dictionary entry not found".into());
    }
    storage
        .list_dictionary()
        .map_err(command_error)?
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| "dictionary entry not found".into())
}

#[tauri::command]
pub(crate) fn add_dictionary_entry(
    entry: DictionaryEntryInput,
    storage: State<'_, Storage>,
) -> Result<DictionaryEntry, String> {
    personalization::validate_dictionary_scope(entry.app_scope.as_deref())
        .map_err(str::to_owned)?;
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
pub(crate) fn import_dictionary_csv(
    input: DictionaryImportInput,
    storage: State<'_, Storage>,
) -> Result<usize, String> {
    storage
        .import_dictionary_csv(&input.csv)
        .map_err(command_error)
}

#[tauri::command]
pub(crate) fn list_dictionary_candidates(
    storage: State<'_, Storage>,
) -> Result<Vec<DictionaryCandidate>, String> {
    storage.list_dictionary_candidates().map_err(command_error)
}

#[tauri::command]
pub(crate) fn confirm_dictionary_candidate(
    id: i64,
    storage: State<'_, Storage>,
) -> Result<DictionaryEntry, String> {
    storage
        .confirm_dictionary_candidate(id)
        .map_err(command_error)?
        .ok_or_else(|| "dictionary candidate not found".into())
}

#[tauri::command]
pub(crate) fn reject_dictionary_candidate(
    id: i64,
    storage: State<'_, Storage>,
) -> Result<(), String> {
    if storage
        .reject_dictionary_candidate(id)
        .map_err(command_error)?
    {
        Ok(())
    } else {
        Err("dictionary candidate not found".into())
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
/// The first report of each stage also replaces the loading message, so a model
/// that is already cached never claims that files are being downloaded.
fn spawn_load_progress_forwarder(
    app: &AppHandle,
    services: &Services,
) -> Option<tauri::async_runtime::JoinHandle<()>> {
    let mut receiver = services.transcriber.load_progress()?;
    let app = app.clone();
    Some(tauri::async_runtime::spawn(async move {
        let mut announced_stage = None;
        loop {
            match receiver.recv().await {
                Ok(progress) => {
                    if announced_stage.as_deref() != Some(progress.stage.as_str()) {
                        if let Some((kind, message)) = load_stage_notice(&progress) {
                            announced_stage = Some(progress.stage.clone());
                            emit_status(&app, kind, message);
                        }
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

/// The notice for the first progress report of a download or verify stage.
/// Only the first download report carries resumed bytes, so it decides
/// whether the download is announced as resumed.
fn load_stage_notice(progress: &asr::LoadProgress) -> Option<(&'static str, &'static str)> {
    match progress.stage.as_str() {
        "download" if progress.resumed_bytes.is_some_and(|bytes| bytes > 0) => Some((
            "model_downloading",
            "Resuming the interrupted speech model download.",
        )),
        "download" => Some((
            "model_downloading",
            "Downloading the speech model files. This runs once; later starts use the local cache.",
        )),
        "verify" => Some((
            "model_verifying",
            "Verifying the downloaded speech model files.",
        )),
        _ => None,
    }
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

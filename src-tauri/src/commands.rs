#[cfg(test)]
use crate::pipeline::{classify_ai_stage, AiStageResult};
#[cfg(test)]
use crate::settings::validate_translation_targets;
use crate::{
    answer_panel, app_context, ask,
    asr::{self, Transcriber},
    audio::{
        self, AudioCapture, AudioDevice, AudioEnhancementConfig, CaptureConfig,
        NoiseSuppressionLevel,
    },
    command_error,
    correction::{self, FailureOutcome},
    emit_ai_failure, emit_stage, emit_state, emit_status,
    injection::{
        self, InjectionOptions, InsertResult, InsertionDetail, SystemTextInjector, TargetWindow,
        TextInjector,
    },
    input_monitor::InputMonitor,
    live_dictation, model_identity, personalization, recording_overlay,
    services::MicrophoneTestState,
    shortcuts,
    state::{AppState, PipelineLifecycle, PipelineMode, PipelinePhase},
    storage::{self, Storage},
    types::{
        self, AppPhase, AppStateSnapshot, DictionaryCandidate, DictionaryEntry,
        DictionaryEntryInput, DictionaryImportInput, GpuDiagnostics, HistoryItem, LoadModelRequest,
        ModelStatus, NewDictionaryEntry, NewHistoryItem, RecordingResult, Settings,
    },
    worker_command_for_settings, AskCapture, AskSession, EditSession, PipelineGuard, Services,
};
#[cfg(test)]
use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::GlobalShortcutExt;

pub(crate) use crate::pipeline::finish::{
    report_history_save_warning, report_temp_cleanup, save_history, save_retry_result,
    HistorySaveStatus,
};

const TRANSCRIPTION_FAILED_SAVED_TO_HISTORY: &str =
    "Transcription failed. The recording was saved to History, where you can retry it.";

const TRANSCRIPTION_FAILED_KEPT_FOR_RETRY: &str =
    "Transcription failed. The recording is kept in History for 24 hours, where you can retry it.";

const TRANSCRIPTION_TOO_LONG_FOR_MODEL: &str = "The recording was too long for this speech model. Retrying with the same model will fail the same way; record shorter clips or switch to faster-whisper.";

const TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY: &str = "The recording was too long for this speech model. It was saved to History, but retrying with the same model will fail the same way; switch to faster-whisper before retrying, or record shorter clips.";

/// Worker error code for a transcript cut off at the model's output limit.
const TRANSCRIPT_TRUNCATED_CODE: &str = "transcript_truncated";

/// Why a take's transcription failed, as far as Retry is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TranscriptionFailure {
    /// Retrying may succeed, for example after a transient worker failure.
    Retryable,
    /// The take exceeds what the speech model can transcribe; Retry with the
    /// same model fails the same way.
    TooLongForModel,
}

impl TranscriptionFailure {
    pub(crate) fn of(error: &asr::AsrError) -> Self {
        match error {
            asr::AsrError::Worker(worker) if worker.code == TRANSCRIPT_TRUNCATED_CODE => {
                Self::TooLongForModel
            }
            _ => Self::Retryable,
        }
    }

    /// `insertion_detail` of the failed take's History row.
    pub(crate) fn history_detail(self) -> Option<&'static str> {
        match self {
            Self::Retryable => None,
            Self::TooLongForModel => Some(TRANSCRIPT_TRUNCATED_CODE),
        }
    }
}

/// `insertion_result` of a History row whose final transcription failed. The
/// row carries no text, only the audio that Retry transcribes again.
const TRANSCRIPTION_FAILED_OUTCOME: &str = "transcription_failed";

/// The History row that lets Retry re-run a take whose transcription failed,
/// or `None` when Retry could not reproduce the request. Ask chooses its
/// action from the transcript, so a take without one has nothing to retry.
pub(crate) fn failed_take_history_item<'a>(
    mode: PipelineMode,
    asr_provider: &'a str,
    edit_source: Option<&'a str>,
    target_language: Option<&'a str>,
    app_category: Option<&'a str>,
    duration_ms: u64,
) -> Option<NewHistoryItem<'a>> {
    // Edit captures a selection, not a target window, so like a successful
    // Edit row it records no app category.
    let (history_mode, source_text, target_language, app_category) = match mode {
        PipelineMode::Dictate => ("faithful", None, None, app_category),
        PipelineMode::Translate => ("translate", None, Some(target_language?), app_category),
        PipelineMode::Edit => ("edit", Some(edit_source?), None, None),
        // Ask chooses its action from the transcript, and a selected-text
        // translation has no recording to retry.
        PipelineMode::Ask | PipelineMode::SelectedText => return None,
    };
    Some(NewHistoryItem {
        transcript_text: "",
        processed_text: None,
        source_text,
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: history_mode,
        asr_provider,
        llm_provider: None,
        target_language,
        app_category,
        duration_ms: Some(duration_ms as i64),
        latency_ms: None,
        retry_of_id: None,
        insertion_result: Some(TRANSCRIPTION_FAILED_OUTCOME),
        insertion_detail: None,
    })
}

/// Saves a take whose transcription failed, with its audio, to History so
/// Retry can transcribe it again. A take is kept where History keeps
/// recordings, or else for 24 hours when keeping failed takes is on. Returns
/// how it was kept, or `None` when it was not.
pub(crate) fn preserve_failed_take(
    storage: &Storage,
    settings: &Settings,
    item: Option<NewHistoryItem<'_>>,
    audio_path: &std::path::Path,
) -> Option<storage::FailedTakeRetention> {
    let item = item?;
    storage::FailedTakeRetention::for_settings(settings)?;
    // Storage re-checks the current settings and writes the row only with
    // its audio: a text-less row without audio would offer nothing to retry.
    storage.add_failed_take(&item, audio_path).ok().flatten()
}

pub(crate) fn failed_take_message(
    kept: Option<storage::FailedTakeRetention>,
    failure: TranscriptionFailure,
) -> &'static str {
    match (failure, kept) {
        (TranscriptionFailure::TooLongForModel, Some(_)) => TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY,
        (TranscriptionFailure::TooLongForModel, None) => TRANSCRIPTION_TOO_LONG_FOR_MODEL,
        (_, Some(storage::FailedTakeRetention::Normal)) => TRANSCRIPTION_FAILED_SAVED_TO_HISTORY,
        (_, Some(storage::FailedTakeRetention::Temporary)) => TRANSCRIPTION_FAILED_KEPT_FOR_RETRY,
        (_, None) => "Transcription failed. Check model and GPU diagnostics.",
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
        // A recording must not fail because a saved device (for example a
        // Bluetooth headset) is away; the saved choice stays unchanged.
        fallback_to_default: true,
        enhancement: AudioEnhancementConfig {
            noise_suppression,
            gain: f32::from(settings.input_gain_percent) / 100.0,
            automatic_gain: settings.automatic_gain,
        },
        ..CaptureConfig::default()
    }
}

pub(crate) fn cancellation_copy(mode: PipelineMode) -> (&'static str, &'static str) {
    match mode {
        PipelineMode::Dictate => ("Dictation cancelled.", "dictation was cancelled"),
        PipelineMode::Translate => ("Translation cancelled.", "translation was cancelled"),
        PipelineMode::Edit => ("Editing cancelled.", "editing was cancelled"),
        PipelineMode::Ask => ("Ask cancelled.", "ask was cancelled"),
        PipelineMode::SelectedText => (
            SELECTED_TEXT_TRANSLATION_CANCELLED,
            "selected-text translation was cancelled",
        ),
    }
}

pub(crate) const EDIT_CLIPBOARD_UNAVAILABLE: &str =
    "The original selection was not changed, and the clipboard is unavailable; the edit is available in this app.";

pub(crate) const INSERTION_FAILED: &str = "Insertion failed; the result is available in this app.";

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

pub(crate) fn persist_edit_completion(
    storage: &Storage,
    item: &NewHistoryItem<'_>,
    audio_path: Option<&std::path::Path>,
    provider: &str,
    elapsed_ms: i64,
) -> (HistorySaveStatus, bool) {
    // The target may already have accepted a paste. Persistence is best effort
    // so a database failure cannot turn that completed edit into command failure.
    let history_status = save_history(storage, item, audio_path);
    let insertion_failed = item.insertion_result == Some("insertion_failed");
    let metric_failed = storage
        .add_metric(
            "speak_to_edit",
            Some(provider),
            Some(elapsed_ms),
            !insertion_failed,
            insertion_failed.then_some("insertion_failed"),
        )
        .is_err();
    (history_status, metric_failed)
}

/// This warning follows, and visually replaces, the completion notice. Only a
/// confirmed paste may be described as a completed edit.
pub(crate) fn edit_persistence_warning(
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

pub(crate) async fn take_published_sessions<T, U>(
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
pub(crate) fn play_stop_cue(enabled: bool) {
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

/// The Settings microphone test opens exactly the chosen device: a silent
/// fallback to the default device would report another microphone's level.
fn microphone_test_config(device_id: Option<String>) -> CaptureConfig {
    CaptureConfig {
        device_id,
        fallback_to_default: false,
        preroll: Duration::ZERO,
        ..CaptureConfig::default()
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
    audio
        .arm(microphone_test_config(device_id))
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

/// Event emitted when the backend ends a microphone test the Settings page
/// did not stop itself (the main window was hidden to the tray).
pub(crate) const MICROPHONE_TEST_STOPPED_EVENT: &str = "microphone-test-stopped";

/// Release a running Settings microphone test; a no-op when none runs.
///
/// Also used when the main window is hidden to the tray: the Settings page
/// stays mounted there, so it would never stop the test itself.
pub(crate) async fn release_running_microphone_test(
    test: &tokio::sync::Mutex<MicrophoneTestState>,
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
) -> Result<bool, audio::AudioError> {
    let mut test = test.lock().await;
    let mut audio = audio.lock().await;
    release_microphone_test(&mut test, audio.as_mut()).await
}

#[tauri::command]
pub(crate) async fn stop_microphone_test(services: State<'_, Services>) -> Result<(), String> {
    release_running_microphone_test(&services.microphone_test, &services.audio)
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

/// What a failed start publishes while it still owns the lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StartFailure {
    /// Leave the published phase: the caller's window reports the error.
    Keep,
    /// Clear a busy phase the start published (the model-preparing state).
    Idle,
    /// Publish Error with the failure, which the overlay shows as a notice.
    Error,
}

/// A hotkey start has no visible window to report a failure, so it always
/// publishes Error; a start from the main window does so only when it had
/// published a busy phase. A cancellation is never reported as a failure.
fn start_failure(
    cancelled: bool,
    from_shortcut: bool,
    published: Option<&AppPhase>,
) -> StartFailure {
    let busy = matches!(
        published,
        Some(AppPhase::Recording | AppPhase::Processing | AppPhase::Injecting)
    );
    match (cancelled, from_shortcut || busy) {
        (true, _) if busy => StartFailure::Idle,
        (true, _) => StartFailure::Keep,
        (false, true) => StartFailure::Error,
        (false, false) => StartFailure::Keep,
    }
}

const START_NO_TARGET: &str =
    "No target window was found. Click where the text should go, then try again.";
const START_SECURE_TARGET: &str =
    "Password and other protected fields cannot receive dictated text.";
const START_ELEVATED_TARGET: &str =
    "The target app runs with higher privileges, so this app cannot type into it.";
const START_MICROPHONE_NOT_FOUND: &str =
    "The selected microphone was not found. Check the microphone in Settings.";
const START_MICROPHONE_UNAVAILABLE: &str =
    "The microphone could not be opened. Check the microphone in Settings.";

/// The fixed, translatable sentence a failed start publishes for a known
/// error kind. Errors arrive as display text (often behind `command_error`'s
/// prefix) and may carry details such as a device id, which a fixed message
/// must not embed. Other errors pass through; the overlay shows a generic
/// translated message for any it does not know.
fn start_failure_message(error: &str) -> String {
    let detail = error
        .strip_prefix("local operation failed: ")
        .unwrap_or(error);
    let known = match detail {
        "no foreground target is available" => Some(START_NO_TARGET),
        "text insertion into secure controls is disabled" => Some(START_SECURE_TARGET),
        "the target cannot be accessed at this privilege level" => Some(START_ELEVATED_TARGET),
        _ if detail.starts_with("audio input device not found") => Some(START_MICROPHONE_NOT_FOUND),
        _ if [
            "microphone capture is supported on Windows only",
            "failed to enumerate audio devices",
            "failed to configure audio device",
            "failed to build input stream",
            "failed to start input stream",
        ]
        .iter()
        .any(|prefix| detail.starts_with(prefix)) =>
        {
            Some(START_MICROPHONE_UNAVAILABLE)
        }
        _ => None,
    };
    known.map_or_else(|| error.to_owned(), str::to_owned)
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
    // Until this take captures its own target, the overlay follows the
    // window the user is working in. Never move a running session's overlay.
    if services.lifecycle.phase() == PipelinePhase::Idle {
        recording_overlay::anchor_to_foreground();
    }
    let claimed = MODEL_CHANGE_GATE.begin_pipeline(|| services.lifecycle.begin_start(mode));
    let operation_id = match claimed {
        Ok(id) => id,
        Err(error) => {
            // Nothing is owned, so only the overlay reports it, and only when
            // no other operation is using the overlay.
            if from_shortcut && services.lifecycle.phase() == PipelinePhase::Idle {
                recording_overlay::show_message_notice(
                    &app,
                    recording_overlay::NoticeSeverity::Failure,
                    error,
                );
            }
            return Err(error.into());
        }
    };
    let guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    let result = start_owned_recording(
        app.clone(),
        services.clone(),
        state.clone(),
        storage.clone(),
        from_shortcut,
        trigger_chord,
        mode,
        operation_id,
    )
    .await;
    match result {
        // The recording keeps the lifecycle until it is stopped or cancelled.
        Ok(()) => std::mem::forget(guard),
        Err(ref error) => {
            let (cancel_message, _) = cancellation_copy(mode);
            match start_failure(
                services.lifecycle.is_cancelled(operation_id),
                from_shortcut,
                state.phase().as_ref(),
            ) {
                StartFailure::Keep => {}
                StartFailure::Idle => emit_state(&app, &state, AppPhase::Idle, cancel_message),
                StartFailure::Error => {
                    emit_state(&app, &state, AppPhase::Error, &start_failure_message(error))
                }
            }
        }
    }
    result
}

/// The saved hotkeys of a recording mode; `None` for selected-text
/// translation, which records nothing.
fn voice_mode_shortcuts(settings: &Settings, mode: PipelineMode) -> Option<&Vec<String>> {
    match mode {
        PipelineMode::Dictate => Some(&settings.shortcuts.dictate),
        PipelineMode::Translate => Some(&settings.shortcuts.translate),
        PipelineMode::Edit => Some(&settings.shortcuts.edit),
        PipelineMode::Ask => Some(&settings.shortcuts.ask),
        PipelineMode::SelectedText => None,
    }
}

/// The `mode` of the overlay's `voice-mode` event.
fn voice_mode_event_name(mode: PipelineMode) -> &'static str {
    match mode {
        PipelineMode::Dictate => "dictate",
        PipelineMode::Translate => "translate",
        PipelineMode::Edit => "edit",
        PipelineMode::Ask => "ask",
        PipelineMode::SelectedText => SELECTED_TEXT_HISTORY_MODE,
    }
}

/// The part of a start that runs while it owns the lifecycle as Starting.
#[allow(clippy::too_many_arguments)]
async fn start_owned_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    from_shortcut: bool,
    trigger_chord: Option<String>,
    mode: PipelineMode,
    operation_id: u64,
) -> Result<(), String> {
    let (cancel_message, cancel_error) = cancellation_copy(mode);
    let settings = storage.get_settings().map_err(command_error)?;
    crate::privacy::check_recording(&settings, mode).map_err(str::to_owned)?;
    state.set_operation(types::OperationKind::Recording, operation_id);
    let mode_shortcuts =
        voice_mode_shortcuts(&settings, mode).ok_or("selected-text translation does not record")?;
    let active_hotkey = trigger_chord
        .as_deref()
        .unwrap_or_else(|| mode_shortcuts[0].as_str());
    let mut edit_session = (mode == PipelineMode::Edit)
        .then(|| EditSession::new(&settings, active_hotkey, from_shortcut))
        .transpose()?;
    let mut ask_session = (mode == PipelineMode::Ask)
        .then(|| AskSession::new(&settings, active_hotkey, from_shortcut));
    // Capture the target at the press: a model load may take minutes, and
    // the foreground window by then is not necessarily the one meant.
    let target = (mode != PipelineMode::Edit && mode != PipelineMode::Ask)
        .then(|| {
            SystemTextInjector::default()
                .capture_target()
                .map(|target| injection::recording_target(target, from_shortcut))
                .map_err(command_error)
        })
        .transpose()?;
    // The overlay appears on the monitor of the window this take targets.
    let ask_target = ask_session
        .as_ref()
        .and_then(|session| match &session.capture {
            AskCapture::Selected(selection) => Some(selection.target()),
            AskCapture::Caret(target) => Some(target),
            AskCapture::Unavailable => None,
        });
    // Without a captured target (Ask with no usable context) the overlay
    // keeps the foreground anchor instead of moving to the primary monitor.
    // A start from this app has no window handle to anchor to, so it keeps
    // the foreground anchor (the app window) too.
    if let Some(target) = target
        .as_ref()
        .filter(|target| !target.is_this_app())
        .or_else(|| {
            edit_session
                .as_ref()
                .map(|session| session.selection.target())
        })
        .or(ask_target)
    {
        recording_overlay::set_anchor(target.window_handle);
    }
    let preparing = services
        .model
        .get()
        .map(|status| needs_model_preparation(&status, &model_identity(&settings).0))
        .unwrap_or(true);
    if preparing {
        emit_stage(
            &app,
            &state,
            types::AppStage::PreparingModel,
            PREPARING_SPEECH_MODEL,
        );
    }
    let cancelled = wait_for_speech_model(
        &app,
        &settings,
        services.lifecycle.cancellation(operation_id)?,
    )
    .await?;
    if cancelled || services.lifecycle.is_cancelled(operation_id) {
        emit_state(&app, &state, AppPhase::Idle, cancel_message);
        return Err(cancel_error.into());
    }
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
    // Every session reports a lost clipboard on drop unless its completion
    // already did, so cancellation and early returns cannot forget it.
    let report_loss = || {
        let app = app.clone();
        move || report_clipboard_not_restored(&app)
    };
    let draft = target.as_ref().map(|target| {
        let mut draft = live_dictation::LiveDraft::new(
            target.clone(),
            &settings,
            active_hotkey,
            mode_shortcuts,
            from_shortcut,
            live_dictation::defers_target_insertion(mode, &settings),
        );
        draft.injector.report_clipboard_loss_on_drop(report_loss());
        draft
    });
    if let Some(session) = edit_session.as_mut() {
        session
            .injector
            .report_clipboard_loss_on_drop(report_loss());
    }
    if let Some(session) = ask_session.as_mut() {
        session
            .injector
            .report_clipboard_loss_on_drop(report_loss());
    }
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
    let microphone_fell_back = audio.uses_fallback_device();
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
            "mode": voice_mode_event_name(mode),
            "targetLanguage": target_language,
        }),
    );
    state.clear_result();
    emit_state(
        &app,
        &state,
        AppPhase::Recording,
        recording_message(mode, microphone_fell_back),
    );
    recording_overlay::set_interactive(&app, mode == PipelineMode::Translate);
    if microphone_fell_back {
        // Shown inside the live view: the recording runs, on another device.
        recording_overlay::show_live_notice(&app, MICROPHONE_FALLBACK_HINT);
        emit_status(&app, "microphone_fallback", MICROPHONE_FALLBACK_STATUS);
    }
    *live_slot = draft.map(|draft| {
        live_dictation::start(
            app.clone(),
            operation_id,
            draft,
            prompt,
            settings.speech_locale.clone(),
            cancel,
            crate::privacy::live_preview_enabled(&settings),
        )
    });
    *edit_slot = edit_session.take();
    *ask_slot = ask_session.take();
    drop(ask_slot);
    drop(edit_slot);
    drop(live_slot);

    let app_for_levels = app.clone();
    tauri::async_runtime::spawn(async move {
        let recording_started = Instant::now();
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
            let (level, stream_failed) = {
                let audio = services.audio.lock().await;
                (audio.level(), audio.stream_error().is_some())
            };
            if let Some(reason) = automatic_stop_reason(
                stream_failed,
                recording_started.elapsed(),
                MAX_RECORDING_DURATION,
            ) {
                // The same path as a user stop: the audio captured so far is
                // transcribed instead of being discarded.
                let _ = stop_recording_for(
                    app_for_levels.clone(),
                    app_for_levels.state::<Services>(),
                    app_for_levels.state::<AppState>(),
                    app_for_levels.state::<Storage>(),
                    reason,
                )
                .await;
                break;
            }
            let _ = app_for_levels.emit("audio-level", level);
        }
    });
    Ok(())
}

/// Flashed in the overlay's live view when a recording uses the default
/// microphone because the saved one is missing. Short enough for that view.
const MICROPHONE_FALLBACK_HINT: &str = "Selected microphone not found. Using the default.";

/// The main window's status for the same fallback.
const MICROPHONE_FALLBACK_STATUS: &str = "The selected microphone was not found, so this recording uses the default microphone. The saved microphone setting was not changed.";

/// The Recording phase message. Dictation names the microphone it records
/// from, which is the default one after a fallback.
fn recording_message(mode: PipelineMode, microphone_fell_back: bool) -> &'static str {
    match mode {
        PipelineMode::Translate => "Recording speech to translate.",
        PipelineMode::Edit => "Recording an edit instruction.",
        PipelineMode::Ask => "Recording an Ask instruction.",
        PipelineMode::Dictate if microphone_fell_back => "Recording from the default microphone.",
        PipelineMode::Dictate | PipelineMode::SelectedText => {
            "Recording from the selected microphone."
        }
    }
}

/// Shown on the overlay while a start waits for the speech model.
const PREPARING_SPEECH_MODEL: &str =
    "Preparing the speech model. Recording starts when it is ready.";

/// Whether a start must wait for a model load, and therefore shows the
/// preparing state instead of appearing to ignore the hotkey.
fn needs_model_preparation(status: &ModelStatus, model_id: &Option<String>) -> bool {
    !(status.state == "ready" && &status.model_id == model_id)
}

/// Waits until the speech model is ready or the start is cancelled, and
/// returns whether it was cancelled first. The load runs as its own task on
/// purpose: cancelling the start only stops this wait (the microphone never
/// opens later), while the load, including a first-use download, keeps going
/// so the next start finds the model ready. Quitting or changing the model
/// still interrupts it.
async fn wait_for_speech_model(
    app: &AppHandle,
    settings: &Settings,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, String> {
    let load = {
        let app = app.clone();
        let settings = settings.clone();
        tauri::async_runtime::spawn(async move {
            let services = app.state::<Services>();
            ensure_model_loaded(&app, &services, &settings)
                .await
                .map(|_| ())
        })
    };
    load_unless_cancelled(async move { load.await.map_err(command_error)? }, cancel).await
}

/// Returns `Ok(true)` as soon as `cancel` is set, without waiting for `load`.
async fn load_unless_cancelled(
    load: impl std::future::Future<Output = Result<(), String>>,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<bool, String> {
    tokio::select! {
        loaded = load => loaded.map(|()| false),
        // A closed channel means the operation is gone, which is a cancel too.
        _ = cancel.wait_for(|cancelled| *cancelled) => Ok(true),
    }
}

/// Recordings stop automatically at this length and are transcribed as usual.
const MAX_RECORDING_DURATION: Duration = Duration::from_secs(15 * 60);

/// Why a recording stopped. Only `User` comes from an explicit request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    User,
    MicrophoneDisconnected,
    MaximumDuration,
}

/// Decides, on each level tick, whether a recording must stop by itself. A
/// failed input stream delivers no more audio, so it wins over the time limit.
fn automatic_stop_reason(
    stream_failed: bool,
    elapsed: Duration,
    limit: Duration,
) -> Option<StopReason> {
    if stream_failed {
        Some(StopReason::MicrophoneDisconnected)
    } else if elapsed >= limit {
        Some(StopReason::MaximumDuration)
    } else {
        None
    }
}

/// Whether the input stream failed during this take, either noticed by the
/// level loop or only by the final capture (a stop within one level tick).
pub(crate) fn microphone_disconnected(
    reason: StopReason,
    artifact: &Result<audio::AudioArtifact, audio::AudioError>,
) -> bool {
    reason == StopReason::MicrophoneDisconnected
        || match artifact {
            Ok(artifact) => artifact.warning.is_some(),
            Err(error) => matches!(error, audio::AudioError::StreamFailure(_)),
        }
}

pub(crate) const MICROPHONE_DISCONNECTED: &str = "The microphone was disconnected, so recording stopped. The audio captured before the disconnection is being transcribed.";
pub(crate) const MICROPHONE_DISCONNECTED_WITHOUT_SPEECH: &str = "The microphone was disconnected before usable speech was captured. Check the microphone and start a new recording.";
pub(crate) const RECORDING_LIMIT_REACHED: &str =
    "Recording reached the 15-minute limit and stopped. The audio is being transcribed.";
const MICROPHONE_DISCONNECTED_PROCESSING: &str =
    "Microphone disconnected. Transcribing what was captured.";
const RECORDING_LIMIT_PROCESSING: &str = "Recording limit reached. Transcribing.";

/// The Processing message the overlay shows. An automatic stop keeps its
/// reason visible there (the status notice appears only in the main window);
/// a user stop shows `default`.
pub(crate) fn processing_message(
    reason: StopReason,
    disconnected: bool,
    default: &'static str,
) -> &'static str {
    if disconnected || reason == StopReason::MicrophoneDisconnected {
        MICROPHONE_DISCONNECTED_PROCESSING
    } else if reason == StopReason::MaximumDuration {
        RECORDING_LIMIT_PROCESSING
    } else {
        default
    }
}

#[tauri::command]
pub(crate) async fn stop_recording(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
) -> Result<RecordingResult, String> {
    stop_recording_for(app, services, state, storage, StopReason::User).await
}

pub(crate) async fn stop_recording_for(
    app: AppHandle,
    services: State<'_, Services>,
    state: State<'_, AppState>,
    storage: State<'_, Storage>,
    reason: StopReason,
) -> Result<RecordingResult, String> {
    crate::pipeline::stop::run(&app, &services, &state, &storage, reason).await
}

/// An output step saw the cancel and stopped before it pasted or kept a
/// copy. The commit stays unsealed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputCancelled;

/// How an output step (a paste or a clipboard copy) run inside the
/// lifecycle's commit ended. The commit makes a cancel either win before
/// anything is pasted or copied, or find the operation committed and do
/// nothing, so a step that ran always completes normally. The lifecycle lock
/// is held for the step, which its paste verification bounds to about a
/// second; lifecycle readers (hotkeys, Esc) wait that long instead of racing
/// the paste. The step must not use the lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommittedOutput<T> {
    Ran(T),
    /// Nothing was pasted or copied.
    Cancelled,
    /// Refused for another reason; nothing was pasted or copied.
    Refused(&'static str),
}

/// `cancelled` is asked only when the step did not run.
fn committed_output<T>(
    committed: Result<Result<T, OutputCancelled>, &'static str>,
    cancelled: impl FnOnce() -> bool,
) -> CommittedOutput<T> {
    match committed {
        Ok(Ok(output)) => CommittedOutput::Ran(output),
        Ok(Err(OutputCancelled)) => CommittedOutput::Cancelled,
        Err(_) if cancelled() => CommittedOutput::Cancelled,
        Err(error) => CommittedOutput::Refused(error),
    }
}

/// Speak to edit's irreversible step: the replacement, or the clipboard copy
/// when it fails before any paste. Returns the outcome (`None`: neither the
/// target nor the clipboard received the edit) and the detail for `None`.
pub(crate) fn commit_edit_replacement(
    lifecycle: &PipelineLifecycle,
    operation_id: u64,
    replace: impl FnOnce() -> Result<InsertResult, injection::InjectionError>,
    copy_to_clipboard: impl FnOnce() -> Result<(), injection::InjectionError>,
) -> CommittedOutput<(Option<InsertResult>, InsertionDetail)> {
    let committed = lifecycle.commit_side_effect(operation_id, || {
        let replaced = replace();
        let failure_detail = if replaced == Err(injection::InjectionError::ClipboardProtected) {
            InsertionDetail::ClipboardProtected
        } else {
            InsertionDetail::ClipboardUnavailable
        };
        Ok((
            edit_insertion_outcome(replaced, copy_to_clipboard),
            failure_detail,
        ))
    });
    committed_output(committed, || lifecycle.is_cancelled(operation_id))
}

/// Dictation's and voice translation's irreversible step: the final paste or
/// a clipboard copy. Only an insertion that saw the cancel, which neither
/// pasted nor copied, leaves the commit unsealed; any other result, an error
/// included, is committed and completes as before. Finishing a live draft
/// verifies its selection and then its paste, about two seconds at most.
pub(crate) fn commit_dictation_output(
    lifecycle: &PipelineLifecycle,
    operation_id: u64,
    output: impl FnOnce() -> Result<InsertResult, injection::InjectionError>,
) -> CommittedOutput<Result<InsertResult, injection::InjectionError>> {
    let committed = lifecycle.commit_side_effect(operation_id, || match output() {
        Err(injection::InjectionError::Cancelled) => Err(OutputCancelled),
        output => Ok(output),
    });
    committed_output(committed, || lifecycle.is_cancelled(operation_id))
}

/// Runs an Ask paste or fallback copy as a committed output step. A step
/// that ran returns its insertion label even if a cancel follows.
pub(crate) fn commit_ask_output(
    lifecycle: &PipelineLifecycle,
    operation_id: u64,
    output: impl FnOnce() -> Result<&'static str, OutputCancelled>,
) -> Result<&'static str, String> {
    match committed_output(lifecycle.commit_side_effect(operation_id, output), || {
        lifecycle.is_cancelled(operation_id)
    }) {
        CommittedOutput::Ran(insertion) => Ok(insertion),
        CommittedOutput::Cancelled => Err("ask was cancelled".into()),
        CommittedOutput::Refused(_) => Err(crate::state::OPERATION_INCOMPLETE.into()),
    }
}

/// Runs an Ask copy whose failure ends the Ask, such as the clarification
/// fallback, inside the lifecycle's commit. A failed copy keeps its error and
/// leaves the operation unsealed.
pub(crate) fn commit_ask_copy(
    lifecycle: &PipelineLifecycle,
    operation_id: u64,
    copy_to_clipboard: impl FnOnce() -> Result<(), injection::InjectionError>,
) -> Result<(), String> {
    match lifecycle.commit_side_effect(operation_id, copy_to_clipboard) {
        Ok(copied) => copied.map_err(command_error),
        Err(_) if lifecycle.is_cancelled(operation_id) => Err("ask was cancelled".into()),
        Err(_) => Err(crate::state::OPERATION_INCOMPLETE.into()),
    }
}

/// The insertion label of an Ask selection replacement or caret draft;
/// `pasted` names a confirmed paste. `None` means the target was not tried.
/// A failure before any paste falls back to the clipboard; a cancel neither
/// pastes nor copies.
pub(crate) fn ask_insertion(
    inserted: Option<Result<InsertResult, injection::InjectionError>>,
    pasted: &'static str,
    copy_to_clipboard: impl FnOnce() -> Result<(), injection::InjectionError>,
) -> Result<&'static str, OutputCancelled> {
    match inserted {
        Some(Ok(InsertResult::ClipboardPaste)) => Ok(pasted),
        Some(Ok(InsertResult::ClipboardOnly)) => Ok("clipboard_only"),
        Some(Ok(InsertResult::PasteUnverified)) => Ok("paste_unverified"),
        Some(Err(injection::InjectionError::Cancelled)) => Err(OutputCancelled),
        Some(Err(_)) | None => Ok(ask_fallback_insertion(copy_to_clipboard())),
    }
}

pub(crate) const CLIPBOARD_NOT_RESTORED: &str =
    "Your previous clipboard contents could not be restored.";

/// Reports a lost user clipboard where no insertion notice is shown (Ask,
/// selected-text translation, cancellation). Restore clipboard was on.
pub(crate) fn report_clipboard_not_restored(app: &AppHandle) {
    emit_status(app, "clipboard_not_restored", CLIPBOARD_NOT_RESTORED);
}

/// `mode` of a selected-text translation's History row (and of its overlay
/// `voice-mode` event).
const SELECTED_TEXT_HISTORY_MODE: &str = "selected_translate";

/// `asr_provider` of a History row that involved no speech recognition.
const NO_ASR_PROVIDER: &str = "none";

/// Metric event recorded once per selected-text translation run.
const SELECTED_TEXT_METRIC: &str = "selected_text_translation";

/// The Processing message while the selection is translated.
const SELECTED_TEXT_TRANSLATING: &str = "Translating the selected text.";

/// The Injecting message while the translation replaces the selection.
const SELECTED_TEXT_REPLACING: &str = "Replacing the original selection.";

const SELECTED_TEXT_TRANSLATION_CANCELLED: &str = "Selected-text translation cancelled.";

const TRANSLATION_KEPT_IN_APP: &str =
    "The translation could not be inserted or copied to the clipboard; it is available in this app.";

const TRANSLATION_FAILED_SELECTION_UNCHANGED: &str =
    "Translation failed; the selected text was not changed.";

/// Why a selected-text translation ended without a translation. Each has a
/// fixed, localized sentence (shown as the overlay's failure notice) and a
/// fixed metric code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedTextFailure {
    SettingsUnavailable,
    ConsentRequired,
    NoSelection,
    MonitorUnavailable,
    TranslationFailed,
    /// The lifecycle refused the replacement without a cancellation.
    Incomplete,
}

impl SelectedTextFailure {
    fn message(self) -> &'static str {
        match self {
            Self::SettingsUnavailable => "Translation settings are unavailable.",
            Self::ConsentRequired => crate::privacy::TEXT_CONSENT_REQUIRED,
            Self::NoSelection => "Select text in a supported foreground edit control.",
            Self::MonitorUnavailable => "Translation could not monitor the target safely.",
            // The selection is unchanged, so there is nothing to recover and
            // no reason to overwrite the user's clipboard with its source.
            Self::TranslationFailed => TRANSLATION_FAILED_SELECTION_UNCHANGED,
            Self::Incomplete => crate::state::OPERATION_INCOMPLETE,
        }
    }

    fn metric_code(self) -> &'static str {
        match self {
            Self::SettingsUnavailable => "settings_unavailable",
            Self::ConsentRequired => "cloud_consent_required",
            Self::NoSelection => "no_selection",
            Self::MonitorUnavailable => "monitor_unavailable",
            Self::TranslationFailed => "translation_failed",
            Self::Incomplete => "operation_incomplete",
        }
    }
}

/// How a selected-text translation ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedTextEnd {
    /// A translation was produced. `None`: it reached neither the target nor
    /// the clipboard and is kept in this app.
    Delivered(Option<InsertResult>),
    Failed(SelectedTextFailure),
    Cancelled,
}

/// The terminal phase and message a selected-text translation publishes.
/// The overlay shows Error as a failure notice and hides on Completed/Idle.
fn selected_text_terminal(end: SelectedTextEnd) -> (AppPhase, &'static str) {
    match end {
        SelectedTextEnd::Delivered(Some(result)) => {
            (AppPhase::Completed, selection_translation_status(result).1)
        }
        SelectedTextEnd::Delivered(None) => (AppPhase::Error, TRANSLATION_KEPT_IN_APP),
        SelectedTextEnd::Failed(failure) => (AppPhase::Error, failure.message()),
        SelectedTextEnd::Cancelled => (AppPhase::Idle, SELECTED_TEXT_TRANSLATION_CANCELLED),
    }
}

/// `insertion_result` of a selected-text translation, with the same codes
/// as Speak to edit's replacement.
fn selected_text_insertion_label(insertion: Option<InsertResult>) -> &'static str {
    match insertion {
        Some(InsertResult::ClipboardPaste) => "clipboard_paste",
        Some(InsertResult::ClipboardOnly) => "clipboard_only",
        Some(InsertResult::PasteUnverified) => "paste_unverified",
        None => "insertion_failed",
    }
}

/// The metric `(success, error_code)` of a run that was not cancelled. Like
/// the voice modes' insertion metric, only a confirmed replacement succeeds;
/// any other end records its outcome or failure code. A cancelled run
/// records no metric, as in the other modes.
fn selected_text_metric(end: SelectedTextEnd) -> Option<(bool, Option<&'static str>)> {
    match end {
        SelectedTextEnd::Delivered(Some(InsertResult::ClipboardPaste)) => Some((true, None)),
        SelectedTextEnd::Delivered(insertion) => {
            Some((false, Some(selected_text_insertion_label(insertion))))
        }
        SelectedTextEnd::Failed(failure) => Some((false, Some(failure.metric_code()))),
        SelectedTextEnd::Cancelled => None,
    }
}

/// The History row of a produced translation: the selection is the source,
/// the translation the processed text. There is no speech, transcript,
/// recording, or duration, so Retry is never offered for it.
#[allow(clippy::too_many_arguments)]
fn selected_text_history_item<'a>(
    source_text: &'a str,
    translation: &'a str,
    llm_provider: &'a str,
    target_language: &'a str,
    app_category: Option<&'a str>,
    latency_ms: u64,
    insertion_result: &'a str,
    insertion_detail: Option<&'a str>,
) -> NewHistoryItem<'a> {
    NewHistoryItem {
        transcript_text: "",
        processed_text: Some(translation),
        source_text: Some(source_text),
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: SELECTED_TEXT_HISTORY_MODE,
        asr_provider: NO_ASR_PROVIDER,
        llm_provider: Some(llm_provider),
        target_language: Some(target_language),
        app_category,
        duration_ms: None,
        latency_ms: Some(latency_ms as i64),
        retry_of_id: None,
        insertion_result: Some(insertion_result),
        insertion_detail,
    }
}

/// A translation paste that was sent but could not be confirmed, for voice
/// and selected-text translation alike.
pub(crate) const TRANSLATION_PASTE_UNVERIFIED: &str = "Translation pasted. This field cannot confirm the result; press Ctrl+V only if the text did not appear (it is on the clipboard).";

/// A translation left on the clipboard because its target changed, for voice
/// and selected-text translation alike.
pub(crate) const TRANSLATION_TARGET_CHANGED: &str =
    "The target changed; the translation remains on the clipboard.";

/// Status kind and message for a selected-text translation outcome. A paste
/// that was sent but not confirmed may already have replaced the selection,
/// so it must not read as a target change that invites a second paste.
fn selection_translation_status(result: InsertResult) -> (&'static str, &'static str) {
    match result {
        InsertResult::ClipboardPaste => ("success", "Translation inserted."),
        InsertResult::PasteUnverified => ("paste_unverified", TRANSLATION_PASTE_UNVERIFIED),
        InsertResult::ClipboardOnly => ("error", TRANSLATION_TARGET_CHANGED),
    }
}

/// Ctrl+Shift+T: translates the selection in the foreground app and replaces
/// it. It owns the lifecycle as a non-recording operation, so voice modes
/// and this translation exclude each other, and Esc, the overlay's cancel
/// button, and `cancel_recording` cancel it. A press while another operation
/// runs is ignored.
pub(crate) async fn translate_selection(app: AppHandle) {
    let services = app.state::<Services>();
    let state = app.state::<AppState>();
    let storage = app.state::<Storage>();
    // Until the selection is captured, the overlay follows the window the
    // user is working in. Never move a running session's overlay.
    if services.lifecycle.phase() == PipelinePhase::Idle {
        recording_overlay::anchor_to_foreground();
    }
    let claimed = MODEL_CHANGE_GATE.begin_pipeline(|| {
        services
            .lifecycle
            .begin_non_recording(PipelineMode::SelectedText)
    });
    let (operation_id, cancel) = match claimed {
        Ok(claimed) => claimed,
        Err(error) => {
            // As for a voice start, only a press that finds the overlay idle
            // (a speech model change) reports why; otherwise it is ignored.
            if services.lifecycle.phase() == PipelinePhase::Idle {
                recording_overlay::show_message_notice(
                    &app,
                    recording_overlay::NoticeSeverity::Failure,
                    error,
                );
                emit_status(&app, "error", error);
            }
            return;
        }
    };
    let _guard = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    // Declared after `_guard`: no exit (a panic included) leaves a busy phase.
    let _phase_guard = crate::state::TerminalPhaseGuard::new(
        &state,
        &services.lifecycle,
        operation_id,
        SELECTED_TEXT_TRANSLATION_CANCELLED,
        |phase, message| emit_state(&app, &state, phase, message),
    );
    let started = Instant::now();
    let mut provider = None;
    let end = run_selected_text_translation(
        &app,
        &services,
        &state,
        &storage,
        operation_id,
        cancel,
        started,
        &mut provider,
    )
    .await;
    if let Some((success, error_code)) = selected_text_metric(end) {
        let _ = storage.add_metric(
            SELECTED_TEXT_METRIC,
            provider.as_deref(),
            Some(started.elapsed().as_millis() as i64),
            success,
            error_code,
        );
    }
    // A delivered translation already published its outcome.
    if !matches!(end, SelectedTextEnd::Delivered(_)) {
        let (phase, message) = selected_text_terminal(end);
        emit_state(&app, &state, phase, message);
    }
}

/// The owned part of `translate_selection`. The caller publishes every end
/// except a delivered translation, which publishes its own outcome.
#[allow(clippy::too_many_arguments)]
async fn run_selected_text_translation(
    app: &AppHandle,
    services: &Services,
    state: &AppState,
    storage: &Storage,
    operation_id: u64,
    cancel: tokio::sync::watch::Receiver<bool>,
    started: Instant,
    provider: &mut Option<String>,
) -> SelectedTextEnd {
    let Ok(settings) = storage.get_settings() else {
        return SelectedTextEnd::Failed(SelectedTextFailure::SettingsUnavailable);
    };
    *provider = Some(settings.correction_provider.clone());
    if crate::privacy::check_text(&settings).is_err() {
        return SelectedTextEnd::Failed(SelectedTextFailure::ConsentRequired);
    }
    let mut injector = SystemTextInjector::new(InjectionOptions {
        restore_clipboard: settings.clipboard_restore,
    });
    // A lost clipboard is reported on every exit unless the outcome below
    // takes it first.
    let app_for_loss = app.clone();
    injector.report_clipboard_loss_on_drop(move || report_clipboard_not_restored(&app_for_loss));
    let selection = injector.capture_selection();
    if services.lifecycle.is_cancelled(operation_id) {
        return SelectedTextEnd::Cancelled;
    }
    let Ok(selection) = selection else {
        return SelectedTextEnd::Failed(SelectedTextFailure::NoSelection);
    };
    // The overlay appears on the monitor of the window that holds the text.
    recording_overlay::set_anchor(selection.target().window_handle);
    let source = selection.text().to_owned();
    let app_context = app_context::from_target(selection.target());
    let _ = app.emit(
        "voice-mode",
        serde_json::json!({
            "mode": voice_mode_event_name(PipelineMode::SelectedText),
            "targetLanguage": settings.translation_target_language,
        }),
    );
    state.clear_result();
    state.set_operation(types::OperationKind::SelectedText, operation_id);
    emit_stage(
        app,
        state,
        types::AppStage::Translating,
        SELECTED_TEXT_TRANSLATING,
    );
    let monitor = InputMonitor::default();
    monitor.observe_cancellation(Some(cancel.clone()));
    if !monitor.start_for_recording(&settings.translation_hotkey, &[], true) {
        return SelectedTextEnd::Failed(SelectedTextFailure::MonitorUnavailable);
    }
    let Some(checkpoint) = monitor.checkpoint() else {
        return SelectedTextEnd::Failed(SelectedTextFailure::MonitorUnavailable);
    };
    if services.lifecycle.is_cancelled(operation_id) {
        return SelectedTextEnd::Cancelled;
    }
    let translated = match correction::translate_text(&settings, &source, cancel).await {
        Ok(translated) => translated,
        Err(_) if services.lifecycle.is_cancelled(operation_id) => {
            return SelectedTextEnd::Cancelled
        }
        Err(correction::CorrectionError::Cancelled) => return SelectedTextEnd::Cancelled,
        Err(_) => return SelectedTextEnd::Failed(SelectedTextFailure::TranslationFailed),
    };
    if services.lifecycle.is_cancelled(operation_id) {
        return SelectedTextEnd::Cancelled;
    }
    emit_state(app, state, AppPhase::Injecting, SELECTED_TEXT_REPLACING);
    // The replacement is the irreversible step. Running it inside the
    // lifecycle's commit makes a cancel either win before anything is pasted
    // or copied, or find the operation committed and do nothing. The
    // lifecycle lock is held for the replacement, which its paste
    // verification bounds to about a second; lifecycle readers (hotkeys,
    // Esc) wait that long instead of racing the paste.
    let committed = services.lifecycle.commit_side_effect(operation_id, || {
        let replaced = injector.replace_selection(&selection, &translated, &monitor, checkpoint);
        // A replacement error happens before any paste, so the selection is
        // unchanged and the clipboard is the fallback.
        let failure_detail = if replaced == Err(injection::InjectionError::ClipboardProtected) {
            InsertionDetail::ClipboardProtected
        } else {
            InsertionDetail::ClipboardUnavailable
        };
        let insertion =
            edit_insertion_outcome(replaced, || injector.copy_to_clipboard(&translated));
        Ok::<_, std::convert::Infallible>((insertion, failure_detail))
    });
    let (insertion, failure_detail) = match committed {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(never)) => match never {},
        // Nothing was pasted or copied.
        Err(_) if services.lifecycle.is_cancelled(operation_id) => {
            return SelectedTextEnd::Cancelled
        }
        Err(_) => return SelectedTextEnd::Failed(SelectedTextFailure::Incomplete),
    };
    let clipboard_lost = injector.take_clipboard_not_restored();
    let restore_detail =
        insertion.and_then(|result| InsertionDetail::for_confirmed_paste(result, clipboard_lost));
    // Insertion has returned; persistence is not insertion.
    recording_overlay::set_phase(app, &AppPhase::Completed);
    let insertion_label = selected_text_insertion_label(insertion);
    let insertion_detail = insertion
        .is_none()
        .then_some(failure_detail)
        .or(restore_detail)
        .map(InsertionDetail::code);
    let history_status = save_history(
        storage,
        &selected_text_history_item(
            &source,
            &translated,
            settings.correction_provider.as_str(),
            settings.translation_target_language.as_str(),
            app_context::history_category(app_context.as_ref()),
            started.elapsed().as_millis() as u64,
            insertion_label,
            insertion_detail,
        ),
        None,
    );
    let end = SelectedTextEnd::Delivered(insertion);
    let (phase, message) = selected_text_terminal(end);
    match insertion {
        Some(result) => {
            let snapshot = state.complete(translated, message.into());
            let _ = app.emit("app-state", snapshot);
            let (kind, message) = selection_translation_status(result);
            emit_status(app, kind, message);
            if result != InsertResult::ClipboardPaste || clipboard_lost {
                recording_overlay::show_insertion_notice(
                    app,
                    injection::notice_outcome(insertion_label, clipboard_lost),
                    None,
                );
            }
        }
        None => {
            // Not inserted, and the clipboard was unavailable or protected
            // under Restore clipboard: keep the translation in this app.
            let _ = app.emit("app-state", state.publish_result(translated));
            emit_state(app, state, phase, message);
            recording_overlay::show_insertion_notice(
                app,
                injection::notice_outcome(insertion_label, clipboard_lost),
                insertion_detail,
            );
        }
    }
    report_history_save_warning(app, history_status);
    if clipboard_lost {
        report_clipboard_not_restored(app);
    }
    end
}

/// Where an answer goes when it was neither inserted nor shown in the answer
/// panel. A copy that failed, or was refused because Restore clipboard could
/// not save the user's clipboard, leaves the result only in this app's window.
pub(crate) fn ask_fallback_insertion(
    copied: Result<(), injection::InjectionError>,
) -> &'static str {
    match copied {
        Ok(()) => "clipboard_only",
        Err(injection::InjectionError::ClipboardProtected) => "clipboard_protected",
        Err(_) => "insertion_failed",
    }
}

pub(crate) fn ask_completion_message(insertion: &str) -> &'static str {
    match insertion {
        "paste_unverified" => {
            "Ask insertion could not be confirmed; the result remains on the clipboard."
        }
        "clipboard_only" => "Ask result is on the clipboard.",
        "insertion_failed" => {
            "The Ask result could not be inserted or copied to the clipboard; it is available in this app."
        }
        "clipboard_protected" => {
            "Your clipboard could not be saved for restoring, so it was left unchanged; the Ask result is available in this app."
        }
        _ => "Ask completed.",
    }
}

#[cfg(test)]
mod ask_outcome_tests {
    use super::*;

    #[test]
    fn a_failed_copy_never_claims_the_result_is_on_the_clipboard() {
        use injection::InjectionError;
        assert_eq!(ask_fallback_insertion(Ok(())), "clipboard_only");
        assert_eq!(
            ask_completion_message(ask_fallback_insertion(Ok(()))),
            "Ask result is on the clipboard."
        );
        let failed = Err(InjectionError::ClipboardUnavailable);
        let message = ask_completion_message(ask_fallback_insertion(failed));
        assert!(!message.contains("is on the clipboard"), "{message}");
        assert!(message.contains("available in this app"), "{message}");
        // A copy refused to protect the clipboard says so.
        let protected = Err(InjectionError::ClipboardProtected);
        let message = ask_completion_message(ask_fallback_insertion(protected));
        assert!(message.contains("left unchanged"), "{message}");
        assert!(message.contains("available in this app"), "{message}");
        assert_eq!(ask_completion_message("panel"), "Ask completed.");
        for insertion in ["selection", "caret"] {
            assert_eq!(ask_completion_message(insertion), "Ask completed.");
        }
    }
}

pub(crate) fn require_ask_spoken_input(spoken: &str) -> Result<(), String> {
    if spoken.trim().is_empty() {
        Err("No Ask instruction was captured.".into())
    } else {
        Ok(())
    }
}

pub(crate) fn action_name(action: &ask::AskAction) -> &'static str {
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
pub(crate) fn search_site_name(site: ask::SearchSite) -> &'static str {
    match site {
        ask::SearchSite::Google => "google",
        ask::SearchSite::YouTube => "youtube",
        ask::SearchSite::AmazonJapan => "amazon_japan",
        ask::SearchSite::GitHub => "github",
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn open_fixed_search(url: &str) -> Result<(), String> {
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

pub(crate) fn completion_persistence_warning(
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

const AUTOSTART_UNCHANGED_IN_DEBUG: &str =
    "Debug builds do not change startup at Windows sign-in; only the setting was saved.";

#[derive(Debug, PartialEq, Eq)]
enum AutostartUpdate {
    Unchanged,
    /// Register (`true`) or remove (`false`) the sign-in Run entry.
    Apply(bool),
    SkippedInDebugBuild,
}

/// The autostart plugin registers the currently running executable. A debug
/// executable depends on Vite's dev server and cannot run on its own at
/// sign-in, so a debug build never writes or removes the Run entry owned by
/// an installed/release build, even with LVI_USE_RELEASE_DATA=1 (startup
/// skips it the same way).
fn autostart_update(previous: bool, desired: bool, debug_build: bool) -> AutostartUpdate {
    if previous == desired {
        AutostartUpdate::Unchanged
    } else if debug_build {
        AutostartUpdate::SkippedInDebugBuild
    } else {
        AutostartUpdate::Apply(desired)
    }
}

/// The single persistence step of `update_settings`. Settings and their
/// History retention (including the History-off purge) commit in one SQLite
/// transaction, so a failed settings write cannot erase History and an insert
/// cannot observe the old settings after the purge. Shortcut registrations are
/// rolled back by `shortcuts::update_registrations` when this fails.
/// Returns the new settings revision.
fn persist_settings(storage: &Storage, settings: &Settings) -> Result<u64, String> {
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

pub(crate) fn emit_correction_preview(app: &AppHandle, text: &str, stage: &str) {
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
) -> Result<types::SettingsSnapshot, String> {
    let _settings_writes = storage.lock_settings_writes().map_err(command_error)?;
    let mut settings = storage.get_settings().map_err(command_error)?;
    let current = active.unwrap_or(settings.translation_target_language.as_str());
    settings.translation_target_language =
        next_translation_target(&settings.translation_target_languages, current)
            .ok_or("at least one translation target language is required")?;
    let revision = storage.update_settings(&settings).map_err(command_error)?;
    Ok(types::SettingsSnapshot { settings, revision })
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
    let snapshot = store_next_translation_target(&storage, active.as_deref())?;
    let next = snapshot.settings.translation_target_language.clone();
    *active = Some(next.clone());
    let _ = app.emit("settings-changed", snapshot);
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
    let (cancel_message, _) = cancellation_copy(mode);
    let cancelled = cancel_pipeline_operation(
        &services.lifecycle,
        &services.audio,
        &services.target,
        &services.voice_translation_target,
        &services.live,
        &services.edit,
        &services.ask,
        || emit_state(&app, &state, AppPhase::Idle, cancel_message),
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
    Ok(())
}

/// Runs its callback once when dropped, on every exit path.
struct OnDrop<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for OnDrop<F> {
    fn drop(&mut self) {
        if let Some(callback) = self.0.take() {
            callback();
        }
    }
}

/// Cancels the active operation. A cancelled recording is released here, and
/// `publish_cancelled` runs on every exit (audio or live errors included)
/// before the lifecycle is released, so its Idle phase can neither be skipped
/// nor overwrite a newer recording. Starting and Processing keep their owner,
/// which publishes their terminal phase.
#[allow(clippy::too_many_arguments)]
async fn cancel_pipeline_operation(
    lifecycle: &PipelineLifecycle,
    audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
    target: &Mutex<Option<TargetWindow>>,
    translation_target: &Mutex<Option<String>>,
    live: &tokio::sync::Mutex<Option<live_dictation::LiveTask>>,
    edit: &tokio::sync::Mutex<Option<EditSession>>,
    ask: &tokio::sync::Mutex<Option<AskSession>>,
    publish_cancelled: impl FnOnce(),
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
    // Declared after `_guard`, so it runs first, while this cancel owns the
    // lifecycle.
    let _published = OnDrop(Some(publish_cancelled));
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

/// The completion status when AI correction stopped at an output token
/// limit: the local server's configurable limit or a cloud provider's budget.
pub(crate) fn output_limit_inserted_message(
    error: &correction::CorrectionError,
) -> Option<&'static str> {
    match error {
        correction::CorrectionError::OutputLimit => Some("AI correction stopped at the local output token limit; the original transcript was inserted. Increase the local output token limit."),
        correction::CorrectionError::ProviderOutputLimit => Some("AI correction stopped at the AI provider's output token limit; the original transcript was inserted. Lower the reasoning effort or choose a different model."),
        _ => None,
    }
}

pub(crate) fn ai_command_error(
    app: &AppHandle,
    error: &correction::CorrectionError,
    outcome: FailureOutcome,
) -> String {
    let message = correction::failure_message(error, outcome);
    emit_ai_failure(app, "ai_provider_failed", error, outcome, &message);
    message
}

pub(crate) fn correction_failure_status(error: &correction::CorrectionError) -> String {
    correction::failure_message(error, FailureOutcome::UsingTranscript)
}

pub(crate) fn edit_failure_message(error: &correction::CorrectionError) -> String {
    correction::failure_message(error, FailureOutcome::UnchangedSelection)
}
/// Metric code for a failed correction; validator rejects stay countable.
pub(crate) fn correction_failure_metric_code(error: &correction::CorrectionError) -> &'static str {
    match error {
        correction::CorrectionError::ProtectedContentChanged => "protected_content_changed",
        _ => error.failure_code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_builds_never_change_the_release_autostart_registration() {
        for desired in [false, true] {
            assert_eq!(
                autostart_update(desired, desired, false),
                AutostartUpdate::Unchanged
            );
            assert_eq!(
                autostart_update(desired, desired, true),
                AutostartUpdate::Unchanged
            );
            assert_eq!(
                autostart_update(!desired, desired, false),
                AutostartUpdate::Apply(desired)
            );
            assert_eq!(
                autostart_update(!desired, desired, true),
                AutostartUpdate::SkippedInDebugBuild
            );
        }
    }

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
    fn retry_announces_the_voice_mode_of_its_history_row() {
        let row = |mode: &str, target_language: Option<&str>| HistoryItem {
            id: 1,
            transcript_text: "text".into(),
            processed_text: None,
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: mode.into(),
            asr_provider: "mock".into(),
            llm_provider: None,
            target_language: target_language.map(str::to_owned),
            app_category: None,
            duration_ms: None,
            latency_ms: None,
            created_at: String::new(),
            has_audio: true,
            retry_of_id: None,
            insertion_result: None,
            insertion_detail: None,
            expires_at: None,
        };
        for (mode, target_language, event_mode) in [
            ("faithful", None, "dictate"),
            ("ai_corrected", None, "dictate"),
            ("faithful_fallback", None, "dictate"),
            ("translate", Some("ja"), "translate"),
            ("edit", None, "edit"),
            ("ask", None, "ask"),
        ] {
            let pipeline_mode = history_pipeline_mode(mode).unwrap();
            assert_eq!(
                retry_voice_mode(pipeline_mode, &row(mode, target_language)),
                serde_json::json!({"mode": event_mode, "targetLanguage": target_language}),
                "{mode}"
            );
        }
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
            edit_failure_message(&CorrectionError::ProviderOutputLimit),
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
            classify_ai_stage(Ok("corrected".into())),
            AiStageResult::Corrected(text) if text == "corrected"
        ));
        assert!(matches!(
            classify_ai_stage(Err(correction::CorrectionError::Cancelled)),
            AiStageResult::Cancelled
        ));
        for error in [
            correction::CorrectionError::ProtectedContentChanged,
            correction::CorrectionError::OutputLimit,
            correction::CorrectionError::MissingApiKey("OPENAI_API_KEY".into()),
            correction::CorrectionError::InvalidResponse("empty".into()),
        ] {
            assert!(matches!(
                classify_ai_stage(Err(error)),
                AiStageResult::Fallback(_)
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
            insertion_result: None,
            insertion_detail: None,
            expires_at: None,
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
                insertion_result: None,
                insertion_detail: None,
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
            insertion_result: None,
            insertion_detail: None,
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
            || persist_settings(&storage, &settings).map(drop),
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
            insertion_result: None,
            insertion_detail: None,
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

    /// Speak to edit and Ask deliver their output after `stop_recording_for`
    /// claimed Processing.
    fn processing_after_recording(lifecycle: &PipelineLifecycle, mode: PipelineMode) -> u64 {
        let id = lifecycle.begin_start(mode).unwrap();
        lifecycle.mark_recording(id).unwrap();
        lifecycle.begin_processing().unwrap();
        id
    }

    #[test]
    fn only_an_output_step_that_did_not_run_can_end_cancelled() {
        assert_eq!(
            committed_output(Ok(Ok("selection")), || -> bool {
                panic!("a step that ran is committed")
            }),
            CommittedOutput::Ran("selection")
        );
        assert_eq!(
            committed_output::<&str>(Ok(Err(OutputCancelled)), || false),
            CommittedOutput::Cancelled
        );
        assert_eq!(
            committed_output::<&str>(Err("pipeline operation was cancelled"), || true),
            CommittedOutput::Cancelled
        );
        assert_eq!(
            committed_output::<&str>(Err("pipeline operation was cancelled"), || false),
            CommittedOutput::Refused("pipeline operation was cancelled")
        );
    }

    #[test]
    fn an_edit_cancelled_before_its_replacement_neither_pastes_nor_copies() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Edit);
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
        assert_eq!(
            commit_edit_replacement(
                &lifecycle,
                id,
                || panic!("a cancelled edit must not paste"),
                || panic!("a cancelled edit must not copy"),
            ),
            CommittedOutput::Cancelled
        );
    }

    #[test]
    fn a_committed_edit_keeps_its_outcome_after_a_later_cancel() {
        use injection::InjectionError;
        // (replacement, fallback copy, committed outcome)
        let cases = [
            (
                Ok(InsertResult::ClipboardPaste),
                Ok(()),
                (
                    Some(InsertResult::ClipboardPaste),
                    InsertionDetail::ClipboardUnavailable,
                ),
            ),
            (
                Ok(InsertResult::PasteUnverified),
                Ok(()),
                (
                    Some(InsertResult::PasteUnverified),
                    InsertionDetail::ClipboardUnavailable,
                ),
            ),
            (
                Err(InjectionError::ClipboardUnavailable),
                Ok(()),
                (
                    Some(InsertResult::ClipboardOnly),
                    InsertionDetail::ClipboardUnavailable,
                ),
            ),
            (
                Err(InjectionError::ClipboardProtected),
                Err(InjectionError::ClipboardProtected),
                (None, InsertionDetail::ClipboardProtected),
            ),
        ];
        for (replaced, copied, outcome) in cases {
            let lifecycle = PipelineLifecycle::default();
            let id = processing_after_recording(&lifecycle, PipelineMode::Edit);
            let mut copy_attempted = false;
            let committed = commit_edit_replacement(
                &lifecycle,
                id,
                || replaced.clone(),
                || {
                    copy_attempted = true;
                    copied
                },
            );
            assert_eq!(committed, CommittedOutput::Ran(outcome));
            // Only a failure before any paste falls back to the clipboard.
            assert_eq!(copy_attempted, replaced.is_err());
            // The edit completes: a later cancel changes nothing.
            assert_eq!(lifecycle.cancel().unwrap(), None);
            assert!(!lifecycle.is_cancelled(id));
        }
    }

    #[test]
    fn a_dictation_cancelled_before_its_output_neither_pastes_nor_copies() {
        // Dictate's final paste; voice translation's paste or fallback copy.
        for mode in [PipelineMode::Dictate, PipelineMode::Translate] {
            let lifecycle = PipelineLifecycle::default();
            let id = processing_after_recording(&lifecycle, mode);
            assert_eq!(
                lifecycle.cancel().unwrap(),
                Some((id, PipelinePhase::Processing))
            );
            assert_eq!(
                commit_dictation_output(&lifecycle, id, || {
                    panic!("a cancelled dictation must neither paste nor copy")
                }),
                CommittedOutput::Cancelled
            );
            assert!(lifecycle.is_cancelled(id));
        }
    }

    #[test]
    fn a_dictation_output_that_saw_a_cancel_stays_cancellable() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Dictate);
        assert_eq!(
            commit_dictation_output(&lifecycle, id, || Err(injection::InjectionError::Cancelled)),
            CommittedOutput::Cancelled
        );
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
    }

    #[test]
    fn a_committed_dictation_output_completes_after_a_later_cancel() {
        use injection::test_backend::{self, MockBackend};
        use std::sync::{Arc, Mutex};
        // An insertion error is committed too: it is saved and reported as before.
        for output in [
            Ok(InsertResult::ClipboardPaste),
            Ok(InsertResult::PasteUnverified),
            Ok(InsertResult::ClipboardOnly),
            Err(injection::InjectionError::ClipboardUnavailable),
        ] {
            let lifecycle = PipelineLifecycle::default();
            let id = processing_after_recording(&lifecycle, PipelineMode::Translate);
            assert_eq!(
                commit_dictation_output(&lifecycle, id, || output.clone()),
                CommittedOutput::Ran(output)
            );
            assert_eq!(lifecycle.cancel().unwrap(), None);
            assert!(!lifecycle.is_cancelled(id));
        }
        // The final paste observes the same cancel the lifecycle sends.
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Dictate);
        let cancel = lifecycle.cancellation(id).unwrap();
        let mock = Arc::new(Mutex::new(MockBackend::new()));
        let injector =
            SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default());
        let monitor = InputMonitor::default();
        monitor.test_set_available(true);
        monitor.observe_cancellation(Some(cancel.clone()));
        let checkpoint = monitor.checkpoint().unwrap();
        assert_eq!(
            commit_dictation_output(&lifecycle, id, || {
                injector.insert_final(
                    "final",
                    &test_backend::target(),
                    &monitor,
                    checkpoint,
                    &cancel,
                    false,
                )
            }),
            CommittedOutput::Ran(Ok(InsertResult::ClipboardPaste))
        );
        assert_eq!(lifecycle.cancel().unwrap(), None);
        // History and the insertion diagnosis never see a cancelled monitor.
        assert!(!*cancel.borrow());
        assert!(monitor.unchanged_since(checkpoint));
        assert_eq!(mock.lock().unwrap().content(), "prefix final suffix");
    }

    #[test]
    fn an_ask_insertion_copies_only_after_a_failure_before_any_paste() {
        use injection::InjectionError;
        let no_copy =
            || -> Result<(), InjectionError> { panic!("a paste result or a cancel must not copy") };
        for pasted in ["selection", "caret"] {
            assert_eq!(
                ask_insertion(Some(Ok(InsertResult::ClipboardPaste)), pasted, no_copy),
                Ok(pasted)
            );
        }
        assert_eq!(
            ask_insertion(Some(Ok(InsertResult::ClipboardOnly)), "caret", no_copy),
            Ok("clipboard_only")
        );
        assert_eq!(
            ask_insertion(
                Some(Ok(InsertResult::PasteUnverified)),
                "selection",
                no_copy
            ),
            Ok("paste_unverified")
        );
        assert_eq!(
            ask_insertion(Some(Err(InjectionError::Cancelled)), "selection", no_copy),
            Err(OutputCancelled)
        );
        for inserted in [None, Some(Err(InjectionError::TargetChanged))] {
            assert_eq!(
                ask_insertion(inserted.clone(), "selection", || Ok(())),
                Ok("clipboard_only")
            );
            assert_eq!(
                ask_insertion(inserted.clone(), "caret", || {
                    Err(InjectionError::ClipboardProtected)
                }),
                Ok("clipboard_protected")
            );
            assert_eq!(
                ask_insertion(inserted, "caret", || Err(
                    InjectionError::ClipboardUnavailable
                )),
                Ok("insertion_failed")
            );
        }
    }

    #[test]
    fn an_ask_cancelled_before_its_output_neither_pastes_nor_copies() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        lifecycle.cancel().unwrap();
        let mut delivered = false;
        assert_eq!(
            commit_ask_output(&lifecycle, id, || {
                delivered = true;
                Ok("clipboard_only")
            }),
            Err("ask was cancelled".to_string())
        );
        assert!(!delivered);
        assert!(lifecycle.is_cancelled(id));
    }

    #[test]
    fn an_ask_output_that_saw_a_cancel_stays_cancellable() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        assert_eq!(
            commit_ask_output(&lifecycle, id, || Err(OutputCancelled)),
            Err("ask was cancelled".to_string())
        );
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
    }

    #[test]
    fn a_refused_ask_output_that_was_not_cancelled_reports_an_incomplete_operation() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        assert_eq!(
            commit_ask_output(&lifecycle, id, || Ok("selection")),
            Ok("selection")
        );
        // The operation is already committed, so a second output is refused.
        assert_eq!(
            commit_ask_output(
                &lifecycle,
                id,
                || -> Result<&'static str, OutputCancelled> {
                    panic!("a refused output must not run")
                }
            ),
            Err(crate::state::OPERATION_INCOMPLETE.to_string())
        );
    }

    #[test]
    fn a_cancel_during_an_ask_paste_waits_and_the_ask_completes() {
        use std::sync::{mpsc, Arc};
        let lifecycle = Arc::new(PipelineLifecycle::default());
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        let cancel = lifecycle.cancellation(id).unwrap();
        let (pasting, wait_pasting) = mpsc::channel();
        let (release, wait_release) = mpsc::channel::<()>();
        let inserter = {
            let lifecycle = lifecycle.clone();
            std::thread::spawn(move || {
                commit_ask_output(&lifecycle, id, || {
                    // The paste is queued and still being verified.
                    pasting.send(()).unwrap();
                    wait_release.recv().unwrap();
                    ask_insertion(
                        Some(Ok(InsertResult::PasteUnverified)),
                        "selection",
                        || -> Result<(), injection::InjectionError> {
                            panic!("a queued paste is never followed by a copy")
                        },
                    )
                })
            })
        };
        wait_pasting.recv().unwrap();
        let canceller = {
            let lifecycle = lifecycle.clone();
            std::thread::spawn(move || lifecycle.cancel().unwrap())
        };
        std::thread::sleep(Duration::from_millis(20));
        assert!(!canceller.is_finished(), "a cancel must wait for the paste");
        release.send(()).unwrap();
        assert_eq!(inserter.join().unwrap(), Ok("paste_unverified"));
        assert_eq!(canceller.join().unwrap(), None);
        // `finish_ask` therefore saves History and publishes completion.
        assert!(!*cancel.borrow());
        assert!(!lifecycle.is_cancelled(id));
    }

    #[test]
    fn an_ask_clarification_cancelled_before_its_copy_does_not_copy() {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        lifecycle.cancel().unwrap();
        assert_eq!(
            commit_ask_copy(&lifecycle, id, || panic!("a cancelled Ask must not copy")),
            Err("ask was cancelled".to_string())
        );
    }

    #[test]
    fn an_ask_clarification_copy_commits_and_a_failed_copy_stays_cancellable() {
        use injection::InjectionError;
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        assert_eq!(commit_ask_copy(&lifecycle, id, || Ok(())), Ok(()));
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert!(!lifecycle.is_cancelled(id));

        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
        assert_eq!(
            commit_ask_copy(&lifecycle, id, || Err(InjectionError::ClipboardUnavailable)),
            Err(command_error(InjectionError::ClipboardUnavailable))
        );
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
    }

    #[test]
    fn edit_insertion_failure_retains_generated_text_and_failure_metric() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit-insertion-failure.sqlite");
        let storage = Storage::open(&path).unwrap();
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
            insertion_result: Some("insertion_failed"),
            insertion_detail: Some("clipboard_unavailable"),
        };
        assert_eq!(
            persist_edit_completion(&storage, &item, None, "local", 200),
            (HistorySaveStatus::Complete, false)
        );
        let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].processed_text.as_deref(), Some("short"));
        assert_eq!(rows[0].source_text.as_deref(), Some("long selection"));
        assert_eq!(
            rows[0].insertion_result.as_deref(),
            Some("insertion_failed")
        );
        assert_eq!(
            rows[0].insertion_detail.as_deref(),
            Some("clipboard_unavailable")
        );
        let metric: (bool, String) = rusqlite::Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT success, error_code FROM metrics WHERE event_type = 'speak_to_edit'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(metric, (false, "insertion_failed".into()));
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
        assert_eq!(cycled.settings.translation_target_language, "ja");
        let stored = storage.get_settings_snapshot().unwrap();
        assert_eq!(stored, cycled);
        assert_eq!(
            stored.settings.history_retention,
            types::HistoryRetention::Never
        );
    }

    #[test]
    fn cycling_the_target_advances_the_settings_revision() {
        let storage = Storage::in_memory().unwrap();
        let before = storage.get_settings_snapshot().unwrap();
        let cycled = store_next_translation_target(&storage, None).unwrap();
        assert_eq!(cycled.revision, before.revision + 1);
        assert_eq!(cycled.settings.translation_target_language, "ja");
        assert_eq!(storage.get_settings_snapshot().unwrap(), cycled);
    }

    /// The storage steps of `update_settings`: under the settings-write lock,
    /// apply the patch to the stored settings, validate, and persist.
    fn save_settings_patch(
        storage: &Storage,
        patch: serde_json::Value,
    ) -> Result<types::SettingsSnapshot, String> {
        let _settings_writes = storage.lock_settings_writes().unwrap();
        let previous = storage.get_settings().unwrap();
        let serde_json::Value::Object(patch) = patch else {
            panic!("a patch is a JSON object");
        };
        let settings = apply_settings_patch(&previous, patch)?;
        settings.validate()?;
        let revision = persist_settings(storage, &settings)?;
        Ok(types::SettingsSnapshot { settings, revision })
    }

    #[test]
    fn a_settings_patch_advances_the_revision_and_returns_the_stored_settings() {
        let storage = Storage::in_memory().unwrap();
        let before = storage.get_settings_snapshot().unwrap();
        let saved = save_settings_patch(&storage, serde_json::json!({ "interactionSounds": true }))
            .unwrap();
        assert_eq!(saved.revision, before.revision + 1);
        assert_eq!(
            saved.settings,
            Settings {
                interaction_sounds: true,
                ..before.settings
            }
        );
        assert_eq!(storage.get_settings_snapshot().unwrap(), saved);
    }

    #[test]
    fn changing_speech_endpoints_revokes_old_speech_consents() {
        let mut previous = Settings {
            asr_backend: "openai-compatible".into(),
            ..Settings::default()
        };
        previous.cloud_consents = vec!["openai".into(), crate::privacy::asr_consent_key(&previous)];
        let patch = serde_json::json!({"apiBaseUrl": "https://another.example/v1"});
        let next = apply_settings_patch(&previous, patch.as_object().unwrap().clone()).unwrap();
        assert_eq!(next.cloud_consents, vec!["openai"]);
        let back = serde_json::json!({"apiBaseUrl": previous.api_base_url});
        let restored = apply_settings_patch(&next, back.as_object().unwrap().clone()).unwrap();
        assert_eq!(restored.cloud_consents, vec!["openai"]);
        let local = serde_json::json!({"asrBackend": "faster-whisper"});
        assert_eq!(
            apply_settings_patch(&previous, local.as_object().unwrap().clone())
                .unwrap()
                .cloud_consents,
            vec!["openai"]
        );
    }

    #[test]
    fn a_settings_patch_keeps_a_target_cycled_after_the_window_read_its_settings() {
        let storage = Storage::in_memory().unwrap();
        // The window shows these settings, then a voice translation cycles
        // the target before the window's queued save is sent.
        let shown = storage.get_settings_snapshot().unwrap();
        let cycled = store_next_translation_target(&storage, None).unwrap();
        assert_ne!(
            cycled.settings.translation_target_language,
            shown.settings.translation_target_language
        );
        let saved = save_settings_patch(
            &storage,
            serde_json::json!({ "historyRetention": "never", "theme": "dark" }),
        )
        .unwrap();
        assert_eq!(saved.revision, cycled.revision + 1);
        assert_eq!(
            saved.settings.translation_target_language,
            cycled.settings.translation_target_language
        );
        assert_eq!(
            saved.settings.history_retention,
            types::HistoryRetention::Never
        );
        assert_eq!(saved.settings.theme, "dark");
        assert_eq!(storage.get_settings_snapshot().unwrap(), saved);
    }

    #[test]
    fn a_settings_patch_with_an_unknown_key_or_invalid_value_writes_nothing() {
        let storage = Storage::in_memory().unwrap();
        let before = storage.get_settings_snapshot().unwrap();
        for patch in [
            // A key that is not a settings field, even beside a valid one.
            serde_json::json!({ "theme": "dark", "futureSetting": true }),
            // Legacy scalar mirrors are not settings fields either.
            serde_json::json!({ "hotkey": "Ctrl+Alt+V" }),
            // Values that do not decode.
            serde_json::json!({ "interactionSounds": "yes" }),
            serde_json::json!({ "historyRetention": null }),
            serde_json::json!({ "shortcuts": { "dictate": ["Ctrl+Alt+V"] } }),
            // Values that decode but fail validation.
            serde_json::json!({ "theme": "sepia" }),
            serde_json::json!({ "inputGainPercent": 10 }),
            serde_json::json!({ "translationTargetLanguage": "xx" }),
        ] {
            assert!(
                save_settings_patch(&storage, patch.clone()).is_err(),
                "{patch} was accepted"
            );
            assert_eq!(storage.get_settings_snapshot().unwrap(), before);
        }
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
    async fn hiding_the_main_window_releases_a_running_microphone_test() {
        let disarms = Arc::new(AtomicUsize::new(0));
        let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
            tokio::sync::Mutex::new(Box::new(TestAudio {
                cancel_error: false,
                disarm_error: false,
                disarms: Arc::clone(&disarms),
                stream_error: None,
            }));
        let test = tokio::sync::Mutex::new(MicrophoneTestState::default());
        let generation = test.lock().await.start();
        assert!(release_running_microphone_test(&test, &audio)
            .await
            .unwrap());
        // The meter task stops at its next tick once its generation is stale.
        assert!(!test.lock().await.is_current(generation));
        // Closing again with no test running leaves the device alone.
        assert!(!release_running_microphone_test(&test, &audio)
            .await
            .unwrap());
        assert_eq!(disarms.load(Ordering::SeqCst), 1);
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
        let cancellation = cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &language,
            &live,
            &edit,
            &ask,
            || (),
        );
        tokio::pin!(cancellation);

        assert!(
            tokio::time::timeout(Duration::from_millis(10), cancellation.as_mut())
                .await
                .is_err()
        );
        assert!(cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &language,
            &live,
            &edit,
            &ask,
            || ()
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
        let published = std::cell::Cell::new(None);

        assert!(cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            || published.set(Some(lifecycle.phase())),
        )
        .await
        .is_err());
        // The failed cancel still published its Idle phase, while it owned
        // the lifecycle, so no newer recording could be overwritten.
        assert_eq!(published.get(), Some(PipelinePhase::Recording));
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
            || panic!("Processing keeps its owner, which publishes its phase"),
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
    async fn a_selected_text_translation_cancel_is_left_to_its_owner() {
        let lifecycle = PipelineLifecycle::default();
        let (id, cancel) = MODEL_CHANGE_GATE
            .begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::SelectedText))
            .unwrap();
        assert_eq!(lifecycle.mode(), Some(PipelineMode::SelectedText));
        let result = cancel_pipeline_operation(
            &lifecycle,
            &test_audio(false),
            &Mutex::new(None),
            &Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            &tokio::sync::Mutex::new(None),
            || panic!("the translation publishes its own Idle phase"),
        )
        .await
        .unwrap();
        assert_eq!(result, Some(PipelinePhase::Processing));
        // The request sees the cancel, the replacement is refused, and no
        // voice mode starts until the owner finishes.
        assert!(*cancel.borrow());
        assert!(lifecycle.is_cancelled(id));
        let mut replaced = false;
        assert!(lifecycle
            .commit_side_effect(id, || {
                replaced = true;
                Ok::<(), ()>(())
            })
            .is_err());
        assert!(!replaced);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[test]
    fn a_failed_translation_does_not_claim_the_clipboard() {
        assert!(!TRANSLATION_FAILED_SELECTION_UNCHANGED.contains("clipboard"));
        assert!(TRANSLATION_KEPT_IN_APP.contains("available in this app"));
    }

    #[test]
    fn an_unverified_paste_is_not_reported_as_a_target_change() {
        let (kind, message) = selection_translation_status(InsertResult::PasteUnverified);
        assert_eq!(kind, "paste_unverified");
        assert!(!message.contains("target changed"), "{message}");
        assert!(message.contains("only if the text did not appear"));
        assert_eq!(
            selection_translation_status(InsertResult::ClipboardOnly).1,
            "The target changed; the translation remains on the clipboard."
        );
        assert_eq!(
            selection_translation_status(InsertResult::ClipboardPaste),
            ("success", "Translation inserted.")
        );
    }

    #[test]
    fn selected_text_translation_ends_in_a_terminal_phase() {
        for result in [
            InsertResult::ClipboardPaste,
            InsertResult::PasteUnverified,
            InsertResult::ClipboardOnly,
        ] {
            assert_eq!(
                selected_text_terminal(SelectedTextEnd::Delivered(Some(result))),
                (AppPhase::Completed, selection_translation_status(result).1)
            );
        }
        assert_eq!(
            selected_text_terminal(SelectedTextEnd::Delivered(None)),
            (AppPhase::Error, TRANSLATION_KEPT_IN_APP)
        );
        // Every failure is a fixed sentence the overlay shows as a notice.
        for (failure, message) in [
            (
                SelectedTextFailure::SettingsUnavailable,
                "Translation settings are unavailable.",
            ),
            (
                SelectedTextFailure::NoSelection,
                "Select text in a supported foreground edit control.",
            ),
            (
                SelectedTextFailure::MonitorUnavailable,
                "Translation could not monitor the target safely.",
            ),
            (
                SelectedTextFailure::TranslationFailed,
                TRANSLATION_FAILED_SELECTION_UNCHANGED,
            ),
            (
                SelectedTextFailure::Incomplete,
                crate::state::OPERATION_INCOMPLETE,
            ),
        ] {
            let (phase, published) = selected_text_terminal(SelectedTextEnd::Failed(failure));
            assert_eq!((phase.clone(), published), (AppPhase::Error, message));
            assert_eq!(
                recording_overlay::phase_view(&phase),
                recording_overlay::PhaseView::Notice(recording_overlay::NoticeSeverity::Failure)
            );
        }
        assert_eq!(
            selected_text_terminal(SelectedTextEnd::Cancelled),
            (AppPhase::Idle, SELECTED_TEXT_TRANSLATION_CANCELLED)
        );
        assert_eq!(
            cancellation_copy(PipelineMode::SelectedText).0,
            SELECTED_TEXT_TRANSLATION_CANCELLED
        );
        // An early exit after a cancel publishes the same Idle message.
        assert_eq!(
            crate::state::unfinished_exit_phase(
                &AppPhase::Processing,
                true,
                cancellation_copy(PipelineMode::SelectedText).0
            ),
            Some((AppPhase::Idle, SELECTED_TEXT_TRANSLATION_CANCELLED))
        );
    }

    #[test]
    fn selected_text_translation_records_one_metric_unless_cancelled() {
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Delivered(Some(
                InsertResult::ClipboardPaste
            ))),
            Some((true, None))
        );
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Delivered(Some(
                InsertResult::PasteUnverified
            ))),
            Some((false, Some("paste_unverified")))
        );
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Delivered(Some(
                InsertResult::ClipboardOnly
            ))),
            Some((false, Some("clipboard_only")))
        );
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Delivered(None)),
            Some((false, Some("insertion_failed")))
        );
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Failed(SelectedTextFailure::NoSelection)),
            Some((false, Some("no_selection")))
        );
        assert_eq!(
            selected_text_metric(SelectedTextEnd::Failed(
                SelectedTextFailure::TranslationFailed
            )),
            Some((false, Some("translation_failed")))
        );
        assert_eq!(selected_text_metric(SelectedTextEnd::Cancelled), None);
    }

    #[test]
    fn selected_text_history_rows_hold_the_selection_and_its_translation() {
        let storage = Storage::in_memory().unwrap();
        let item = selected_text_history_item(
            "Guten Morgen",
            "Good morning",
            "openai",
            "en",
            Some("browser"),
            1234,
            selected_text_insertion_label(Some(InsertResult::ClipboardOnly)),
            Some("user_activity"),
        );
        assert_eq!(
            save_history(&storage, &item, None),
            HistorySaveStatus::Complete
        );
        let row = storage
            .list_history(types::HistoryFilter::Translate, 10)
            .unwrap()
            .remove(0);
        assert_eq!(row.mode, "selected_translate");
        assert_eq!(row.source_text.as_deref(), Some("Guten Morgen"));
        assert_eq!(row.processed_text.as_deref(), Some("Good morning"));
        assert_eq!(row.transcript_text, "");
        assert_eq!(row.instruction_text, None);
        assert_eq!(row.asr_provider, NO_ASR_PROVIDER);
        assert_eq!(row.llm_provider.as_deref(), Some("openai"));
        assert_eq!(row.target_language.as_deref(), Some("en"));
        assert_eq!(row.app_category.as_deref(), Some("browser"));
        assert_eq!(row.duration_ms, None);
        assert_eq!(row.latency_ms, Some(1234));
        assert_eq!(row.insertion_result.as_deref(), Some("clipboard_only"));
        assert_eq!(row.insertion_detail.as_deref(), Some("user_activity"));
        assert!(!row.has_audio);
        // It has no recording, so Retry refuses it.
        assert_eq!(
            history_pipeline_mode(&row.mode),
            Err("history mode is not retryable".to_string())
        );
        assert!(
            failed_take_history_item(PipelineMode::SelectedText, "mock", None, None, None, 1)
                .is_none()
        );
        assert_eq!(
            selected_text_insertion_label(None),
            "insertion_failed",
            "the same code Speak to edit stores"
        );
    }

    #[test]
    fn selected_text_history_honours_history_off() {
        let storage = Storage::in_memory().unwrap();
        persist_settings(
            &storage,
            &Settings {
                history_retention: types::HistoryRetention::Never,
                ..Settings::default()
            },
        )
        .unwrap();
        let item = selected_text_history_item(
            "source",
            "translation",
            "local",
            "ja",
            None,
            1,
            "clipboard_paste",
            None,
        );
        assert_eq!(
            save_history(&storage, &item, None),
            HistorySaveStatus::Complete
        );
        assert!(storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn only_recordings_fall_back_to_the_default_microphone() {
        let settings = Settings {
            microphone_id: Some("name:Headset".into()),
            ..Settings::default()
        };
        let recording = capture_config(&settings);
        assert_eq!(recording.device_id.as_deref(), Some("name:Headset"));
        assert!(recording.fallback_to_default);
        let test = microphone_test_config(Some("name:Headset".into()));
        assert_eq!(test.device_id.as_deref(), Some("name:Headset"));
        assert!(!test.fallback_to_default);
        assert_eq!(test.preroll, Duration::ZERO);
    }

    #[test]
    fn a_fallback_recording_names_the_default_microphone() {
        assert_eq!(
            recording_message(PipelineMode::Dictate, false),
            "Recording from the selected microphone."
        );
        assert_eq!(
            recording_message(PipelineMode::Dictate, true),
            "Recording from the default microphone."
        );
        for mode in [
            PipelineMode::Translate,
            PipelineMode::Edit,
            PipelineMode::Ask,
        ] {
            assert_eq!(
                recording_message(mode, true),
                recording_message(mode, false)
            );
        }
        assert!(!MICROPHONE_FALLBACK_HINT.is_empty());
        assert!(MICROPHONE_FALLBACK_STATUS.contains("was not changed"));
    }

    #[test]
    fn selected_text_translation_has_no_recording_shortcuts() {
        let settings = Settings::default();
        assert!(voice_mode_shortcuts(&settings, PipelineMode::SelectedText).is_none());
        assert_eq!(
            voice_mode_shortcuts(&settings, PipelineMode::Ask),
            Some(&settings.shortcuts.ask)
        );
        assert_eq!(
            voice_mode_event_name(PipelineMode::SelectedText),
            SELECTED_TEXT_HISTORY_MODE
        );
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
            &lifecycle,
            &audio,
            &target,
            &language,
            &live,
            &edit,
            &ask,
            || ()
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
            cancel_pipeline_operation(
                &lifecycle,
                &audio,
                &target,
                &language,
                &live,
                &edit,
                &ask,
                || ()
            )
            .await
            .unwrap(),
            Some(PipelinePhase::Processing)
        );
        assert_eq!(language.lock().unwrap().as_deref(), Some("ja"));
        lifecycle.finish(id);

        let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
        lifecycle.mark_recording(id).unwrap();
        assert_eq!(
            cancel_pipeline_operation(
                &lifecycle,
                &audio,
                &target,
                &language,
                &live,
                &edit,
                &ask,
                || ()
            )
            .await
            .unwrap(),
            Some(PipelinePhase::Recording)
        );
        assert_eq!(*language.lock().unwrap(), None);
    }

    #[test]
    fn correction_failure_status_excludes_provider_response_body() {
        for status in [
            reqwest::StatusCode::UNAUTHORIZED,
            reqwest::StatusCode::FORBIDDEN,
        ] {
            let error = correction::CorrectionError::Api {
                status,
                message: "synthetic-provider-secret".into(),
            };
            let message = correction_failure_status(&error);
            assert!(message.contains("rejected the API key"));
            assert!(message.contains("Using the original transcript."));
            assert!(!message.contains("synthetic-provider-secret"));
            assert!(
                edit_failure_message(&error).contains("The original selection was not changed.")
            );
        }
        let invalid = correction::CorrectionError::InvalidApiKey {
            status: reqwest::StatusCode::BAD_REQUEST,
        };
        assert!(correction_failure_status(&invalid).contains("rejected the API key"));
        let error = correction::CorrectionError::Api {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: "synthetic-provider-secret".into(),
        };
        let message = correction_failure_status(&error);
        assert!(message.contains("provider returned an error"));
        assert!(!message.contains("synthetic-provider-secret"));
    }

    #[test]
    fn correction_failure_status_distinguishes_the_output_limit() {
        let local = correction_failure_status(&correction::CorrectionError::OutputLimit);
        let cloud = correction_failure_status(&correction::CorrectionError::ProviderOutputLimit);
        assert!(local.contains("local AI model"));
        assert!(cloud.contains("AI provider"));
        assert_ne!(local, cloud);
        for message in [&local, &cloud] {
            assert!(message.contains("output token limit"));
            assert!(message.contains("Using the original transcript."));
        }
        assert!(
            output_limit_inserted_message(&correction::CorrectionError::OutputLimit)
                .unwrap()
                .contains("the original transcript was inserted")
        );
        assert!(
            output_limit_inserted_message(&correction::CorrectionError::ProviderOutputLimit)
                .unwrap()
                .contains("the original transcript was inserted")
        );
        assert_eq!(
            output_limit_inserted_message(&correction::CorrectionError::Cancelled),
            None
        );
    }

    #[test]
    fn protected_content_rejection_has_distinct_status_and_metric() {
        let error = correction::CorrectionError::ProtectedContentChanged;
        assert!(correction_failure_status(&error).contains("protected"));
        assert_eq!(
            correction_failure_metric_code(&error),
            "protected_content_changed"
        );
        let invalid =
            correction::CorrectionError::InvalidResponse("synthetic-provider-secret".into());
        assert_eq!(
            correction_failure_metric_code(&invalid),
            "ai_invalid_response"
        );
        assert!(!correction_failure_status(&invalid).contains("synthetic-provider-secret"));
    }
    #[test]
    fn a_failed_microphone_or_the_time_limit_stops_the_recording() {
        let limit = MAX_RECORDING_DURATION;
        assert_eq!(limit, Duration::from_secs(15 * 60));
        assert_eq!(
            automatic_stop_reason(false, Duration::from_secs(60), limit),
            None
        );
        assert_eq!(
            automatic_stop_reason(true, Duration::from_secs(1), limit),
            Some(StopReason::MicrophoneDisconnected)
        );
        assert_eq!(
            automatic_stop_reason(false, limit, limit),
            Some(StopReason::MaximumDuration)
        );
        assert_eq!(
            automatic_stop_reason(true, limit * 2, limit),
            Some(StopReason::MicrophoneDisconnected)
        );
    }

    #[test]
    fn a_stream_failure_is_reported_even_when_the_user_stopped_first() {
        let artifact = |warning: Option<&str>| audio::AudioArtifact {
            path: PathBuf::from("take.wav"),
            sample_rate: 24_000,
            channels: 1,
            sample_count: 24_000,
            duration: Duration::from_secs(1),
            peak_level: 0.5,
            rms_level: 0.1,
            vad: audio::VadAnalysis::default(),
            warning: warning.map(str::to_owned),
        };
        assert!(!microphone_disconnected(
            StopReason::User,
            &Ok(artifact(None))
        ));
        assert!(microphone_disconnected(
            StopReason::User,
            &Ok(artifact(Some("device disconnected")))
        ));
        assert!(microphone_disconnected(
            StopReason::MicrophoneDisconnected,
            &Ok(artifact(None))
        ));
        assert!(microphone_disconnected(
            StopReason::User,
            &Err(audio::AudioError::StreamFailure(
                "device disconnected".into()
            ))
        ));
        assert!(!microphone_disconnected(
            StopReason::MaximumDuration,
            &Err(audio::AudioError::NoVoiceDetected)
        ));
    }

    #[test]
    fn an_automatic_stop_keeps_its_reason_in_the_overlay() {
        assert_eq!(
            processing_message(StopReason::User, false, "Transcribing locally."),
            "Transcribing locally."
        );
        assert_eq!(
            processing_message(StopReason::User, true, "Transcribing locally."),
            MICROPHONE_DISCONNECTED_PROCESSING
        );
        assert_eq!(
            processing_message(StopReason::MicrophoneDisconnected, false, "Stopping"),
            MICROPHONE_DISCONNECTED_PROCESSING
        );
        assert_eq!(
            processing_message(StopReason::MaximumDuration, false, "Stopping"),
            RECORDING_LIMIT_PROCESSING
        );
        assert_eq!(
            processing_message(StopReason::MaximumDuration, true, "Stopping"),
            MICROPHONE_DISCONNECTED_PROCESSING
        );
    }

    #[test]
    fn a_hotkey_start_failure_is_published_for_the_overlay() {
        let idle = Some(&AppPhase::Idle);
        let preparing = Some(&AppPhase::Processing);
        // A hotkey has no visible window: the failure must reach the overlay.
        assert_eq!(start_failure(false, true, idle), StartFailure::Error);
        assert_eq!(start_failure(false, true, None), StartFailure::Error);
        // The main window reports its own command error, unless the start
        // had already shown the preparing state.
        assert_eq!(start_failure(false, false, idle), StartFailure::Keep);
        assert_eq!(start_failure(false, false, preparing), StartFailure::Error);
        // A cancellation is not a failure; it only clears a busy phase.
        assert_eq!(start_failure(true, true, idle), StartFailure::Keep);
        assert_eq!(start_failure(true, true, preparing), StartFailure::Idle);
        assert_eq!(start_failure(true, false, preparing), StartFailure::Idle);
        assert_eq!(
            start_failure(true, false, Some(&AppPhase::Completed)),
            StartFailure::Keep
        );
    }

    #[test]
    fn only_a_start_that_must_load_the_model_shows_the_preparing_state() {
        let status = |state: &str, model: &str| ModelStatus {
            model_id: Some(model.into()),
            quantization: "4bit".into(),
            installed: true,
            state: state.into(),
            detail: String::new(),
        };
        let current = Some("faster-whisper:large-v3-turbo".to_string());
        let ready = status("ready", "faster-whisper:large-v3-turbo");
        assert!(!needs_model_preparation(&ready, &current));
        for state in ["loading", "not_loaded", "error"] {
            assert!(needs_model_preparation(
                &status(state, "faster-whisper:large-v3-turbo"),
                &current
            ));
        }
        // A ready model that is not the configured one is reloaded first.
        assert!(needs_model_preparation(
            &status("ready", "faster-whisper:small"),
            &current
        ));
    }

    #[tokio::test]
    async fn cancelling_while_the_model_loads_never_starts_the_recording() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        let cancel = lifecycle.cancellation(id).unwrap();
        let (finish_load, load_finished) = tokio::sync::oneshot::channel::<()>();
        let waiting = load_unless_cancelled(
            async move {
                let _ = load_finished.await;
                Ok(())
            },
            cancel,
        );
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), waiting.as_mut())
                .await
                .is_err()
        );
        // A second press cancels the Starting operation mid-load.
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Starting))
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), waiting)
                .await
                .unwrap(),
            Ok(true)
        );
        // The load finishing later cannot open the microphone.
        let _ = finish_load.send(());
        assert!(lifecycle.mark_recording(id).is_err());
        lifecycle.finish(id);
        assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
    }

    #[tokio::test]
    async fn a_ready_model_starts_without_waiting_for_cancellation() {
        let lifecycle = PipelineLifecycle::default();
        let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
        let cancel = lifecycle.cancellation(id).unwrap();
        assert_eq!(
            load_unless_cancelled(async { Ok(()) }, cancel.clone()).await,
            Ok(false)
        );
        assert_eq!(
            load_unless_cancelled(async { Err("load failed".into()) }, cancel).await,
            Err("load failed".to_string())
        );
    }

    #[test]
    fn known_start_errors_become_fixed_translatable_messages() {
        let wrapped = |detail: &str| format!("local operation failed: {detail}");
        assert_eq!(
            start_failure_message(&wrapped("no foreground target is available")),
            START_NO_TARGET
        );
        assert_eq!(
            start_failure_message(&wrapped("text insertion into secure controls is disabled")),
            START_SECURE_TARGET
        );
        assert_eq!(
            start_failure_message(&wrapped(
                "the target cannot be accessed at this privilege level"
            )),
            START_ELEVATED_TARGET
        );
        // The device id is not part of the message.
        let missing = start_failure_message(&wrapped(
            "audio input device not found: {0.0.1.00000000}.{abc}",
        ));
        assert_eq!(missing, START_MICROPHONE_NOT_FOUND);
        assert!(!missing.contains("abc"));
        assert_eq!(
            start_failure_message(&wrapped("failed to build input stream: busy")),
            START_MICROPHONE_UNAVAILABLE
        );
        // Already fixed sentences and unknown errors pass through.
        assert_eq!(
            start_failure_message("Select text in a supported foreground edit control."),
            "Select text in a supported foreground edit control."
        );
        assert_eq!(
            start_failure_message(&wrapped("worker exited unexpectedly")),
            wrapped("worker exited unexpectedly")
        );
    }

    fn model_change() -> (Settings, Settings) {
        let previous = Settings::default();
        let mut next = previous.clone();
        next.model_id = Some("custom/model".into());
        (previous, next)
    }

    #[test]
    fn a_recording_cannot_start_between_the_model_check_and_reconfigure() {
        let gate = ModelChangeGate::new();
        let lifecycle = PipelineLifecycle::default();
        let (previous, next) = model_change();

        // update_settings: the Idle check and the mark happen in one step.
        let change = gate
            .begin_change(&previous, &next, || lifecycle.phase())
            .unwrap();
        assert!(change.is_some());
        assert_eq!(
            gate.begin_pipeline(|| lifecycle.begin_start(PipelineMode::Dictate)),
            Err(MODEL_CHANGE_IN_PROGRESS)
        );
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);

        // After the reconfiguration the start proceeds, and a later model
        // change observes the claimed pipeline and is refused.
        drop(change);
        assert!(gate
            .begin_pipeline(|| lifecycle.begin_start(PipelineMode::Dictate))
            .is_ok());
        assert_eq!(
            gate.begin_change(&previous, &next, || lifecycle.phase())
                .err(),
            Some(MODEL_CHANGE_WHILE_BUSY.to_string())
        );
        // Unrelated settings never take the mark, even while recording.
        assert!(gate
            .begin_change(&previous, &previous, || lifecycle.phase())
            .unwrap()
            .is_none());
    }

    #[test]
    fn overlapping_model_changes_are_refused() {
        let gate = ModelChangeGate::new();
        let (previous, next) = model_change();
        let first = gate
            .begin_change(&previous, &next, || PipelinePhase::Idle)
            .unwrap();
        assert!(first.is_some());
        // A stale save reverting the model while the first change is pending.
        assert_eq!(
            gate.begin_change(&next, &previous, || PipelinePhase::Idle)
                .err(),
            Some(MODEL_CHANGE_IN_PROGRESS.to_string())
        );
        // The refused change did not clear the first change's mark.
        assert_eq!(
            gate.begin_pipeline(|| Ok(())),
            Err(MODEL_CHANGE_IN_PROGRESS)
        );
        drop(first);
        assert_eq!(gate.begin_pipeline(|| Ok(())), Ok(()));
        assert!(gate
            .begin_change(&next, &previous, || PipelinePhase::Idle)
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_start_during_a_model_change_fails_fast_instead_of_waiting() {
        let gate = Arc::new(ModelChangeGate::new());
        let lifecycle = PipelineLifecycle::default();
        let (previous, next) = model_change();
        let (marked, wait_marked) = std::sync::mpsc::channel();
        let (release, wait_release) = std::sync::mpsc::channel::<()>();
        // A reconfiguration that is still running (for example behind a
        // model download that holds the transcriber).
        let updater = std::thread::spawn({
            let gate = Arc::clone(&gate);
            move || {
                let _change = gate
                    .begin_change(&previous, &next, || PipelinePhase::Idle)
                    .unwrap();
                marked.send(()).unwrap();
                wait_release.recv().unwrap();
            }
        });
        wait_marked.recv().unwrap();
        let started = Instant::now();
        assert_eq!(
            gate.begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::Dictate))
                .map(|(id, _)| id),
            Err(MODEL_CHANGE_IN_PROGRESS)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
        release.send(()).unwrap();
        updater.join().unwrap();
        // The guard cleared the mark when the update finished.
        assert!(gate
            .begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::Dictate))
            .is_ok());
    }

    #[test]
    fn model_changes_wait_for_an_idle_pipeline() {
        let previous = Settings::default();
        let mut backend = previous.clone();
        backend.asr_backend = if previous.asr_backend == "mock" {
            "faster-whisper"
        } else {
            "mock"
        }
        .into();
        let mut model = previous.clone();
        model.model_id = Some("custom/model".into());
        let mut quantization = previous.clone();
        quantization.model_quantization = if previous.model_quantization == "4bit" {
            "8bit"
        } else {
            "4bit"
        }
        .into();
        assert_ne!(previous.asr_backend, "openai-compatible");
        let mut api = previous.clone();
        api.asr_backend = "openai-compatible".into();
        let mut endpoint = api.clone();
        endpoint.api_base_url = "https://asr.example.invalid/v1".into();
        // The API fields do not reach a local backend's worker.
        let mut local_endpoint = previous.clone();
        local_endpoint.api_base_url = "https://asr.example.invalid/v1".into();
        local_endpoint.api_key_env_var = "OTHER_ASR_API_KEY".into();
        let mut unrelated = previous.clone();
        unrelated.interaction_sounds = !previous.interaction_sounds;
        for busy in [
            PipelinePhase::Starting,
            PipelinePhase::Recording,
            PipelinePhase::Processing,
        ] {
            for (from, next) in [
                (&previous, &backend),
                (&previous, &model),
                (&previous, &quantization),
                (&api, &endpoint),
            ] {
                assert_eq!(
                    reject_model_change_while_busy(from, next, busy),
                    Err(MODEL_CHANGE_WHILE_BUSY.to_string())
                );
                assert_eq!(
                    reject_model_change_while_busy(from, next, PipelinePhase::Idle),
                    Ok(())
                );
            }
            for next in [&unrelated, &local_endpoint] {
                assert_eq!(
                    reject_model_change_while_busy(&previous, next, busy),
                    Ok(())
                );
            }
        }
    }

    #[test]
    fn only_a_different_worker_command_counts_as_a_worker_change() {
        let local = Settings::default();
        assert_ne!(local.asr_backend, "openai-compatible");
        let mut api = local.clone();
        api.asr_backend = "openai-compatible".into();
        let changed = |edit: fn(&mut Settings), from: &Settings| {
            let mut next = from.clone();
            edit(&mut next);
            (from.clone(), next)
        };
        let unchanged = [
            changed(
                |next| next.api_base_url = "https://asr.example.invalid/v1".into(),
                &local,
            ),
            changed(
                |next| next.api_key_env_var = "OTHER_ASR_API_KEY".into(),
                &local,
            ),
            changed(|next| next.model_id = Some("  ".into()), &local),
            changed(
                |next| next.api_base_url = format!(" {} ", next.api_base_url),
                &api,
            ),
            changed(
                |next| next.interaction_sounds = !next.interaction_sounds,
                &api,
            ),
        ];
        for (previous, next) in &unchanged {
            assert!(!worker_configuration_changed(previous, next));
            assert!(!speech_model_changed(previous, next));
            // Skipping reconfigure is sound: the worker command is the same.
            assert_eq!(
                worker_command_for_settings(previous),
                worker_command_for_settings(next)
            );
        }
        let worker_changes = [
            changed(|next| next.model_quantization = "8bit".into(), &local),
            changed(|next| next.asr_backend = "openai-compatible".into(), &local),
            changed(|next| next.model_id = Some("custom/model".into()), &local),
            changed(
                |next| next.api_base_url = "https://asr.example.invalid/v1".into(),
                &api,
            ),
            changed(
                |next| next.api_key_env_var = "OTHER_ASR_API_KEY".into(),
                &api,
            ),
        ];
        for (previous, next) in &worker_changes {
            assert!(worker_configuration_changed(previous, next));
            assert!(speech_model_changed(previous, next));
        }
    }

    fn failed_take_storage(settings: Settings) -> (Storage, tempfile::TempDir, PathBuf) {
        let storage = Storage::in_memory().unwrap();
        storage.update_settings(&settings).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let take = directory.path().join("failed-take.wav");
        std::fs::write(&take, b"RIFF failed take").unwrap();
        (storage, directory, take)
    }

    /// History and its audio are kept, so a failed take is a normal row.
    fn keeping_audio() -> Settings {
        Settings {
            history_retention: types::HistoryRetention::OneMonth,
            delete_audio_after_processing: false,
            ..Settings::default()
        }
    }

    /// The most private settings: no History and no audio.
    fn keeping_nothing(keep_failed_takes: bool) -> Settings {
        Settings {
            history_retention: types::HistoryRetention::Never,
            delete_audio_after_processing: true,
            keep_failed_takes,
            ..Settings::default()
        }
    }

    fn dictate_failure() -> Option<NewHistoryItem<'static>> {
        failed_take_history_item(PipelineMode::Dictate, "mock", None, None, None, 1)
    }

    fn history_audio_count(storage: &Storage) -> usize {
        storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .iter()
            .filter(|row| row.has_audio)
            .count()
    }

    #[test]
    fn a_failed_transcription_is_kept_in_history_for_retry() {
        let (storage, _directory, take) = failed_take_storage(keeping_audio());
        let settings = storage.get_settings().unwrap();
        let item = failed_take_history_item(
            PipelineMode::Dictate,
            "faster-whisper:large-v3-turbo",
            None,
            None,
            Some("messaging"),
            90_000,
        );
        let kept = preserve_failed_take(&storage, &settings, item, &take);
        assert_eq!(kept, Some(storage::FailedTakeRetention::Normal));
        assert_eq!(
            failed_take_message(kept, TranscriptionFailure::Retryable),
            TRANSCRIPTION_FAILED_SAVED_TO_HISTORY
        );

        let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(row.has_audio);
        assert_eq!(row.expires_at, None, "normal History retention applies");
        assert_eq!(row.transcript_text, "");
        assert_eq!(
            row.insertion_result.as_deref(),
            Some(TRANSCRIPTION_FAILED_OUTCOME)
        );
        assert_eq!(row.duration_ms, Some(90_000));
        assert_eq!(history_pipeline_mode(&row.mode), Ok(PipelineMode::Dictate));
        let retry_copy = storage.copy_history_audio_for_retry(row.id).unwrap();
        assert_eq!(std::fs::read(&retry_copy).unwrap(), b"RIFF failed take");
        std::fs::remove_file(retry_copy).unwrap();
        // History keeps its own copy; the caller still removes its temporary take.
        assert!(take.is_file());
        storage.delete_all_history().unwrap();
    }

    #[test]
    fn a_failed_take_is_kept_for_retry_even_when_history_and_audio_are_off() {
        for settings in [
            keeping_nothing(true),
            Settings {
                delete_audio_after_processing: false,
                ..keeping_nothing(true)
            },
            Settings {
                history_retention: types::HistoryRetention::OneMonth,
                ..keeping_nothing(true)
            },
        ] {
            let (storage, _directory, take) = failed_take_storage(settings.clone());
            let kept = preserve_failed_take(&storage, &settings, dictate_failure(), &take);
            assert_eq!(kept, Some(storage::FailedTakeRetention::Temporary));
            assert_eq!(
                failed_take_message(kept, TranscriptionFailure::Retryable),
                TRANSCRIPTION_FAILED_KEPT_FOR_RETRY
            );

            let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
            assert_eq!(rows.len(), 1, "{settings:?}");
            let row = &rows[0];
            assert!(row.has_audio);
            assert!(row.expires_at.is_some());
            let retry_copy = storage.copy_history_audio_for_retry(row.id).unwrap();
            assert_eq!(std::fs::read(&retry_copy).unwrap(), b"RIFF failed take");
            std::fs::remove_file(retry_copy).unwrap();
            storage.delete_all_history().unwrap();
        }
    }

    #[test]
    fn without_keeping_failed_takes_only_history_with_audio_keeps_them() {
        for settings in [
            keeping_nothing(false),
            Settings {
                delete_audio_after_processing: false,
                ..keeping_nothing(false)
            },
            Settings {
                history_retention: types::HistoryRetention::OneMonth,
                ..keeping_nothing(false)
            },
        ] {
            let (storage, _directory, take) = failed_take_storage(settings.clone());
            let kept = preserve_failed_take(&storage, &settings, dictate_failure(), &take);
            assert_eq!(kept, None, "{settings:?}");
            assert_eq!(
                failed_take_message(kept, TranscriptionFailure::Retryable),
                "Transcription failed. Check model and GPU diagnostics."
            );
            assert!(storage
                .list_history(types::HistoryFilter::All, 10)
                .unwrap()
                .is_empty());
        }

        let settings = Settings {
            keep_failed_takes: false,
            ..keeping_audio()
        };
        let (storage, _directory, take) = failed_take_storage(settings.clone());
        assert_eq!(
            preserve_failed_take(&storage, &settings, dictate_failure(), &take),
            Some(storage::FailedTakeRetention::Normal)
        );
        storage.delete_all_history().unwrap();
    }

    #[test]
    fn a_failed_take_is_never_saved_without_its_audio() {
        // Audio retention and keeping failed takes were turned off while the
        // take was being transcribed.
        let (storage, _directory, take) = failed_take_storage(Settings {
            history_retention: types::HistoryRetention::OneMonth,
            ..keeping_nothing(false)
        });
        let snapshot = keeping_audio();
        assert_eq!(
            preserve_failed_take(&storage, &snapshot, dictate_failure(), &take),
            None
        );
        assert!(storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .is_empty());

        // The audio copy fails (here: the take no longer exists).
        for settings in [keeping_audio(), keeping_nothing(true)] {
            let (storage, directory, _take) = failed_take_storage(settings.clone());
            let missing = directory.path().join("missing.wav");
            assert_eq!(
                preserve_failed_take(&storage, &settings, dictate_failure(), &missing),
                None
            );
            assert!(storage
                .list_history(types::HistoryFilter::All, 10)
                .unwrap()
                .is_empty());
        }
    }

    fn retry_result(source_id: i64) -> NewHistoryItem<'static> {
        NewHistoryItem {
            transcript_text: "retried",
            processed_text: None,
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: "faithful",
            asr_provider: "mock",
            llm_provider: None,
            target_language: None,
            app_category: None,
            duration_ms: Some(1),
            latency_ms: Some(1),
            retry_of_id: Some(source_id),
            insertion_result: None,
            insertion_detail: None,
        }
    }

    #[test]
    fn a_successful_retry_applies_the_normal_settings_to_a_failed_take() {
        // History off: neither the failed take nor the Retry result remains.
        let (storage, directory, take) = failed_take_storage(keeping_nothing(true));
        let settings = storage.get_settings().unwrap();
        preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
        let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
        let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
        assert_eq!(
            save_retry_result(&storage, &retry_result(source), &retry_audio, source),
            HistorySaveStatus::Complete
        );
        std::fs::remove_file(retry_audio).unwrap();
        assert!(storage.history_item(source).unwrap().is_none());
        assert!(storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
        drop(directory);

        // History on, audio deleted: the Retry text is kept without audio.
        let (storage, _directory, take) = failed_take_storage(Settings {
            history_retention: types::HistoryRetention::OneMonth,
            ..keeping_nothing(true)
        });
        let settings = storage.get_settings().unwrap();
        preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
        let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
        let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
        save_retry_result(&storage, &retry_result(source), &retry_audio, source);
        std::fs::remove_file(retry_audio).unwrap();
        assert!(storage.history_item(source).unwrap().is_none());
        let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transcript_text, "retried");
        assert_eq!(history_audio_count(&storage), 0);

        // History with audio enabled since the failure: the take stays as a
        // normal row instead of expiring.
        let (storage, _directory, take) = failed_take_storage(keeping_nothing(true));
        let settings = storage.get_settings().unwrap();
        preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
        let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
        storage
            .update_settings_and_apply_history_policy(&keeping_audio())
            .unwrap();
        let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
        save_retry_result(&storage, &retry_result(source), &retry_audio, source);
        std::fs::remove_file(retry_audio).unwrap();
        let kept = storage.history_item(source).unwrap().unwrap();
        assert!(kept.has_audio);
        assert_eq!(kept.expires_at, None);
        assert_eq!(history_audio_count(&storage), 2);
        storage.delete_all_history().unwrap();
    }

    #[test]
    fn a_truncated_transcript_is_not_presented_as_retryable() {
        let worker = |code: &str| {
            asr::AsrError::Worker(asr::WorkerError {
                code: code.into(),
                message: "failed".into(),
            })
        };
        assert_eq!(
            TranscriptionFailure::of(&worker(TRANSCRIPT_TRUNCATED_CODE)),
            TranscriptionFailure::TooLongForModel
        );
        for error in [
            worker("transcription_failed"),
            worker("cuda_oom"),
            asr::AsrError::Timeout,
            asr::AsrError::Crashed,
            asr::AsrError::Protocol("transcript_truncated".into()),
        ] {
            assert_eq!(
                TranscriptionFailure::of(&error),
                TranscriptionFailure::Retryable,
                "{error}"
            );
        }

        let too_long = TranscriptionFailure::TooLongForModel;
        assert_eq!(too_long.history_detail(), Some("transcript_truncated"));
        assert_eq!(TranscriptionFailure::Retryable.history_detail(), None);
        assert_eq!(
            failed_take_message(None, too_long),
            TRANSCRIPTION_TOO_LONG_FOR_MODEL
        );
        for kept in [
            storage::FailedTakeRetention::Normal,
            storage::FailedTakeRetention::Temporary,
        ] {
            assert_eq!(
                failed_take_message(Some(kept), too_long),
                TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY
            );
        }
        for message in [
            TRANSCRIPTION_TOO_LONG_FOR_MODEL,
            TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY,
        ] {
            assert!(message.contains("will fail the same way"));
            assert!(message.contains("faster-whisper"));
        }
    }

    #[test]
    fn a_truncated_failed_take_records_why_in_history() {
        let (storage, _directory, take) = failed_take_storage(keeping_nothing(true));
        let settings = storage.get_settings().unwrap();
        let item = dictate_failure().map(|item| NewHistoryItem {
            insertion_detail: TranscriptionFailure::TooLongForModel.history_detail(),
            ..item
        });
        assert!(preserve_failed_take(&storage, &settings, item, &take).is_some());
        let row = &storage.list_history(types::HistoryFilter::All, 1).unwrap()[0];
        assert_eq!(
            row.insertion_result.as_deref(),
            Some(TRANSCRIPTION_FAILED_OUTCOME)
        );
        assert_eq!(
            row.insertion_detail.as_deref(),
            Some("transcript_truncated")
        );
        storage.delete_all_history().unwrap();
    }

    #[test]
    fn failed_takes_keep_what_retry_needs_for_each_mode() {
        let translate = failed_take_history_item(
            PipelineMode::Translate,
            "mock",
            None,
            Some("English"),
            None,
            1,
        )
        .unwrap();
        assert_eq!(translate.mode, "translate");
        assert_eq!(translate.target_language, Some("English"));
        let edit = failed_take_history_item(
            PipelineMode::Edit,
            "mock",
            Some("selected"),
            None,
            Some("messaging"),
            1,
        )
        .unwrap();
        assert_eq!(edit.mode, "edit");
        assert_eq!(edit.source_text, Some("selected"));
        assert_eq!(edit.app_category, None);
        for mode in ["faithful", "translate", "edit"] {
            assert!(history_pipeline_mode(mode).is_ok());
        }
        // Retry could not reproduce these takes, so they are not kept.
        assert!(
            failed_take_history_item(PipelineMode::Translate, "mock", None, None, None, 1)
                .is_none()
        );
        assert!(
            failed_take_history_item(PipelineMode::Edit, "mock", None, None, None, 1).is_none()
        );
        assert!(failed_take_history_item(
            PipelineMode::Ask,
            "mock",
            Some("selected"),
            None,
            None,
            1
        )
        .is_none());
    }
}

#[tauri::command]
pub(crate) fn get_settings(storage: State<'_, Storage>) -> Result<types::SettingsSnapshot, String> {
    storage.get_settings_snapshot().map_err(command_error)
}

#[tauri::command]
pub(crate) fn get_ui_preferences(storage: State<'_, Storage>) -> Result<serde_json::Value, String> {
    let snapshot = storage.get_settings_snapshot().map_err(command_error)?;
    Ok(
        serde_json::json!({"revision": snapshot.revision, "settings": {
            "uiLanguage": snapshot.settings.ui_language, "theme": snapshot.settings.theme
        }}),
    )
}

#[tauri::command]
pub(crate) fn get_shortcut_warning(services: State<'_, Services>) -> Vec<String> {
    services
        .inactive_shortcuts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Names the stored settings fields this build could not read and replaces
/// in memory (content-free JSON keys such as `historyRetention` or
/// `shortcuts.ask`, or `unreadable` for the whole row). The Settings page
/// fetches this on mount, so the notice does not depend on a startup event.
#[tauri::command]
pub(crate) fn get_settings_repair_status(
    storage: State<'_, Storage>,
) -> Result<Vec<String>, String> {
    storage
        .settings_repair_status()
        .map(|repairs| repairs.fields())
        .map_err(command_error)
}

/// Saves the current values of the replaced fields, so a user who wants the
/// fallback values keeps them. Returns the fields still replaced afterwards.
#[tauri::command]
pub(crate) async fn persist_repaired_settings(
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<Vec<String>, String> {
    let _update_guard = services.settings_update.lock().await;
    let _settings_writes = storage.lock_settings_writes().map_err(command_error)?;
    storage.persist_repaired_settings().map_err(command_error)?;
    storage
        .settings_repair_status()
        .map(|repairs| repairs.fields())
        .map_err(command_error)
}

/// Applies `patch`, the changed top-level settings keys in their JSON form,
/// to `stored`, so a save never rewrites keys it does not name. A key that is
/// not a settings field, or a value that does not decode, rejects the patch.
fn apply_settings_patch(
    stored: &Settings,
    patch: serde_json::Map<String, serde_json::Value>,
) -> Result<Settings, String> {
    let explicit_consent = patch.contains_key("cloudConsents");
    let serde_json::Value::Object(mut merged) =
        serde_json::to_value(stored).map_err(|error| error.to_string())?
    else {
        return Err("stored settings are not an object".into());
    };
    for (key, value) in patch {
        let Some(field) = merged.get_mut(&key) else {
            return Err(format!("unknown settings field: {key}"));
        };
        *field = value;
    }
    let mut settings: Settings = serde_json::from_value(serde_json::Value::Object(merged))
        .map_err(|error| format!("invalid settings value: {error}"))?;
    if settings.asr_backend != stored.asr_backend
        || crate::privacy::asr_consent_key(&settings) != crate::privacy::asr_consent_key(stored)
    {
        let current = crate::privacy::asr_consent_key(&settings);
        settings.cloud_consents.retain(|key| {
            !key.starts_with("asr:")
                || (explicit_consent
                    && settings.asr_backend == "openai-compatible"
                    && *key == current)
        });
    }
    Ok(settings)
}

/// Saves `patch` (see `apply_settings_patch`) over the stored settings and
/// returns them with their new revision, which `settings-changed` carries
/// too.
#[tauri::command]
pub(crate) async fn update_settings(
    app: AppHandle,
    patch: serde_json::Map<String, serde_json::Value>,
    services: State<'_, Services>,
    storage: State<'_, Storage>,
) -> Result<types::SettingsSnapshot, String> {
    let _update_guard = services.settings_update.lock().await;
    // The command is async, so waiting here never blocks the main thread
    // needed by global-shortcut registration. Keep this guard from reading
    // the settings the patch applies to through the single transactional
    // settings/History persistence step.
    let (previous, settings, revision, _model_change) = {
        let _settings_writes = storage.lock_settings_writes().map_err(command_error)?;
        let previous = storage.get_settings().map_err(command_error)?;
        let settings = apply_settings_patch(&previous, patch)?;
        settings.validate()?;
        crate::environment::register_provider_keys(&settings);
        if !previous.setup_complete
            && settings.setup_complete
            && !is_prepared_model(&services.model.get()?, &settings)
        {
            return Err("Load the selected speech model before finishing setup.".into());
        }
        // Held until the transcriber is reconfigured and its status reset;
        // recording starts and Retry fail fast meanwhile instead of waiting.
        let model_change =
            MODEL_CHANGE_GATE.begin_change(&previous, &settings, || services.lifecycle.phase())?;
        let old_routes = services
            .shortcut_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let desired_routes =
            shortcuts::desired_routes_for_update(&previous, &settings, &old_routes)?;
        if cfg!(debug_assertions) && settings.auto_start && !previous.auto_start {
            return Err(
            "autostart cannot be enabled from a development build; install and run a release build"
                .into(),
        );
        }
        // Held until the new routes are published, so the Esc cancel chord
        // follower cannot re-register Esc from the old routes in between.
        let mut cancel_chord = services
            .cancel_chord_registered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let phase = services.lifecycle.phase();
        let mut revision = None;
        let registration = shortcuts::update_registrations_with_cancel_chord(
            &mut cancel_chord,
            phase,
            &old_routes,
            &desired_routes,
            |chord| app.global_shortcut().register(chord),
            |chord| app.global_shortcut().unregister(chord),
            || {
                revision = Some(persist_settings(&storage, &settings)?);
                Ok(())
            },
        );
        registration?;
        // Registration succeeds only after the settings were persisted.
        let revision = revision.ok_or("settings were not saved")?;
        let inactive = shortcuts::inactive_descriptions(&settings, &desired_routes)?;
        *services
            .shortcut_routes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = desired_routes;
        drop(cancel_chord);
        *services
            .inactive_shortcuts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = inactive;
        (previous, settings, revision, model_change)
    };
    match autostart_update(
        previous.auto_start,
        settings.auto_start,
        cfg!(debug_assertions),
    ) {
        AutostartUpdate::Unchanged => {}
        AutostartUpdate::SkippedInDebugBuild => {
            emit_status(
                &app,
                "autostart_unchanged_in_debug",
                AUTOSTART_UNCHANGED_IN_DEBUG,
            );
        }
        AutostartUpdate::Apply(enable) => {
            let result = if enable {
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
    }
    let model_configuration_changed = worker_configuration_changed(&previous, &settings);
    if model_configuration_changed {
        services
            .transcriber
            .reconfigure(worker_command_for_settings(&settings))
            .await;
    }
    if model_configuration_changed {
        let (model_id, detail) = model_identity(&settings);
        let status = ModelStatus {
            model_id,
            quantization: settings.model_quantization.clone(),
            installed: false,
            state: "not_loaded".into(),
            detail,
        };
        // A settings save takes the status from any load still running.
        services.model.replace(status.clone())?;
        let _ = app.emit("model-status", status);
    }
    let snapshot = types::SettingsSnapshot { settings, revision };
    let _ = app.emit("settings-changed", &snapshot);
    Ok(snapshot)
}

/// Settings that require a new worker command (`reconfigure` kills the worker).
fn worker_configuration_changed(previous: &Settings, next: &Settings) -> bool {
    worker_command_inputs(previous) != worker_command_inputs(next)
        || previous.model_quantization != next.model_quantization
}

/// The settings `worker_command_for_settings` derives the worker command
/// from, normalized the same way. The API endpoint only reaches the
/// openai-compatible worker, so editing it for a local backend keeps the
/// worker and its loaded model.
fn worker_command_inputs(settings: &Settings) -> (&str, Option<&str>, Option<(&str, &str)>) {
    let model_id = settings
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let api_endpoint = (settings.asr_backend == "openai-compatible").then(|| {
        (
            settings.api_base_url.trim(),
            settings.api_key_env_var.trim(),
        )
    });
    (settings.asr_backend.as_str(), model_id, api_endpoint)
}

const MODEL_CHANGE_WHILE_BUSY: &str =
    "Finish the current recording or processing before changing the speech model.";
const MODEL_CHANGE_IN_PROGRESS: &str = "The speech model is being changed. Try again in a moment.";

/// Makes a speech-model change atomic with respect to pipeline starts.
///
/// `update_settings` checks that the pipeline is idle and marks a change in
/// progress in one step under this short-lived lock, then reconfigures the
/// transcriber without it. Recording start and Retry claim the lifecycle
/// under the same lock and fail fast while the mark is set. A start can
/// therefore neither slip between the check and the reconfiguration nor wait
/// behind a model load that holds the transcriber.
struct ModelChangeGate {
    changing: Mutex<bool>,
}

pub(crate) fn begin_history_retry(
    services: &Services,
    mode: PipelineMode,
) -> Result<(u64, tokio::sync::watch::Receiver<bool>), String> {
    MODEL_CHANGE_GATE
        .begin_pipeline(|| services.lifecycle.begin_non_recording(mode))
        .map_err(str::to_string)
}

static MODEL_CHANGE_GATE: ModelChangeGate = ModelChangeGate::new();

impl ModelChangeGate {
    const fn new() -> Self {
        Self {
            changing: Mutex::new(false),
        }
    }

    fn changing(&self) -> std::sync::MutexGuard<'_, bool> {
        self.changing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Returns a guard that keeps starts out until it drops, or `None` when
    /// `next` does not change the speech model.
    fn begin_change(
        &self,
        previous: &Settings,
        next: &Settings,
        phase: impl FnOnce() -> PipelinePhase,
    ) -> Result<Option<ModelChangeGuard<'_>>, String> {
        let mut changing = self.changing();
        if !speech_model_changed(previous, next) {
            return Ok(None);
        }
        // A second change while one is pending (for example a stale save
        // carrying the old model) would let the first guard clear the mark
        // while its own reconfiguration is still pending.
        if *changing {
            return Err(MODEL_CHANGE_IN_PROGRESS.into());
        }
        reject_model_change_while_busy(previous, next, phase())?;
        *changing = true;
        Ok(Some(ModelChangeGuard { gate: self }))
    }

    /// Claims the pipeline with `begin` unless a model change is in progress.
    fn begin_pipeline<T>(
        &self,
        begin: impl FnOnce() -> Result<T, &'static str>,
    ) -> Result<T, &'static str> {
        let changing = self.changing();
        if *changing {
            return Err(MODEL_CHANGE_IN_PROGRESS);
        }
        begin()
    }
}

/// Clears the model-change mark on every exit path of `update_settings`.
struct ModelChangeGuard<'a> {
    gate: &'a ModelChangeGate,
}

impl Drop for ModelChangeGuard<'_> {
    fn drop(&mut self) {
        *self.gate.changing() = false;
    }
}

fn speech_model_changed(previous: &Settings, next: &Settings) -> bool {
    worker_configuration_changed(previous, next)
}

/// A recording or its processing owns the loaded speech model until it
/// finishes. Reconfiguring or reloading it underneath would fail the final
/// transcription, so such changes wait until the pipeline is idle.
fn reject_model_change_while_busy(
    previous: &Settings,
    next: &Settings,
    phase: PipelinePhase,
) -> Result<(), String> {
    if speech_model_changed(previous, next) && phase != PipelinePhase::Idle {
        return Err(MODEL_CHANGE_WHILE_BUSY.into());
    }
    Ok(())
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

/// Read-only count shown in the confirmation before a shorter retention is
/// saved; saving it purges through the same cutoff and row selection.
#[tauri::command]
pub(crate) fn preview_history_retention_purge(
    retention: types::HistoryRetention,
    storage: State<'_, Storage>,
) -> Result<storage::HistoryPurgePreview, String> {
    storage
        .history_purge_preview(retention)
        .map_err(command_error)
}

/// The recording's WAV bytes as a raw IPC body (an `ArrayBuffer` in the
/// webview), not a JSON number array; the frontend names the file from `id`.
#[tauri::command]
pub(crate) fn get_history_audio(
    id: i64,
    storage: State<'_, Storage>,
) -> Result<tauri::ipc::Response, String> {
    storage
        .history_audio(id)
        .map_err(command_error)?
        .map(tauri::ipc::Response::new)
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
    crate::pipeline::retry::run(id, &app, &services, &state, &storage).await
}

/// The `voice-mode` event of a Retry: the mode of the History row it re-runs.
pub(crate) fn retry_voice_mode(mode: PipelineMode, source: &HistoryItem) -> serde_json::Value {
    serde_json::json!({
        "mode": voice_mode_event_name(mode),
        "targetLanguage": (mode == PipelineMode::Translate)
            .then_some(source.target_language.as_deref())
            .flatten(),
    })
}

pub(crate) fn history_pipeline_mode(mode: &str) -> Result<PipelineMode, String> {
    match mode {
        "translate" => Ok(PipelineMode::Translate),
        "edit" => Ok(PipelineMode::Edit),
        "ask" => Ok(PipelineMode::Ask),
        "faithful" | "ai_corrected" | "faithful_fallback" => Ok(PipelineMode::Dictate),
        _ => Err("history mode is not retryable".into()),
    }
}

pub(crate) fn retry_dictate_history_mode(
    llm_provider: Option<&str>,
    correction_failed: bool,
) -> &'static str {
    if llm_provider.is_some() {
        "ai_corrected"
    } else if correction_failed {
        "faithful_fallback"
    } else {
        "faithful"
    }
}

pub(crate) fn retry_app_context(item: &HistoryItem) -> Option<types::AppContext> {
    item.app_category.clone().map(|category| types::AppContext {
        app_key: None,
        category,
    })
}

pub(crate) fn stored_ask_action(item: &HistoryItem) -> Result<ask::AskAction, String> {
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

pub(crate) fn stored_search_site(item: &HistoryItem) -> Result<ask::SearchSite, String> {
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
    services.model.get()
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
    ensure_model_loaded_cancellable(app, services, settings, None).await
}

pub(crate) const MODEL_LOAD_CANCELLED: &str = "Speech model loading was cancelled.";

fn is_prepared_model(status: &ModelStatus, settings: &Settings) -> bool {
    status.installed
        && status.state == "ready"
        && status.model_id == model_identity(settings).0
        && status.quantization == settings.model_quantization
}

/// `ensure_model_loaded` for a pipeline operation: cancelling the operation
/// abandons a load (or first-use download) that nobody waits for any more.
pub(crate) async fn ensure_model_loaded_cancellable(
    app: &AppHandle,
    services: &Services,
    settings: &Settings,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> Result<ModelStatus, String> {
    crate::privacy::check_asr(settings).map_err(str::to_owned)?;
    let (model_id, detail) = model_identity(settings);
    // A reset or restoring worker does not hold the model, whatever the
    // status says.
    let resident = services
        .transcriber
        .model_residency()
        .is_none_or(|residency| matches!(*residency.borrow(), asr::ModelResidency::Resident(_)));
    if let Ok(status) = services.model.get() {
        if resident && is_prepared_model(&status, settings) {
            return Ok(status);
        }
    }
    emit_status(app, "model_loading", "Loading the speech model.");
    let loading = ModelStatus {
        model_id: model_id.clone(),
        quantization: settings.model_quantization.clone(),
        installed: false,
        state: "loading".into(),
        detail: detail.clone(),
    };
    let ready = ModelStatus {
        model_id,
        quantization: settings.model_quantization.clone(),
        installed: true,
        state: "ready".into(),
        detail: format!(
            "{detail} Loaded with {} quantization.",
            settings.model_quantization
        ),
    };

    // Only the worker knows whether the model files are already cached, so the
    // download message and progress bar are driven by what it reports.
    let progress_forwarder = spawn_load_progress_forwarder(app, services);
    let publish = |status: ModelStatus| {
        let _ = app.emit("model-status", status);
    };
    let (result, shown_by) = load_and_publish_status(
        &services.model,
        services.transcriber.as_ref(),
        &settings.model_quantization,
        cancel,
        ModelLoadStatuses {
            loading: &loading,
            ready: &ready,
            detail: &detail,
        },
        &publish,
    )
    .await;
    if let Some(forwarder) = progress_forwarder {
        forwarder.abort();
    }
    if let Some(load) = shown_by {
        follow_model_residency(app, services, load, &ready, detail);
    }
    result.map(|()| ready)
}

/// The statuses one load may publish.
struct ModelLoadStatuses<'a> {
    loading: &'a ModelStatus,
    ready: &'a ModelStatus,
    /// Detail of the model without the "Loaded with" suffix.
    detail: &'a str,
}

/// Load the model and publish its status, returning the load's ownership
/// id when its `ready` status is shown.
///
/// A load takes over the status only once the transcriber starts it (has
/// its worker lock). A load that waits behind another one, for example a
/// recording start during the startup download, and is cancelled meanwhile
/// therefore never touches the status, and the running load still shows its
/// own outcome.
async fn load_and_publish_status(
    slot: &ModelStatusSlot,
    transcriber: &dyn Transcriber,
    quantization: &str,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
    statuses: ModelLoadStatuses<'_>,
    publish: &(dyn Fn(ModelStatus) + Send + Sync),
) -> (Result<(), String>, Option<u64>) {
    let owner = Mutex::new(None);
    let started = || {
        if let Ok(load) = slot.begin_load(statuses.loading.clone()) {
            *owner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(load);
            publish(statuses.loading.clone());
        }
    };
    let result = transcriber
        .load_when_started(quantization, cancel, &started)
        .await;
    let owner = owner
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Every outcome is shown only while this load still owns the status: a
    // newer load or a settings save may already show its own.
    let show = |status: ModelStatus| {
        let shown = owner.and_then(|load| slot.update(load, status));
        if let Some(status) = shown.clone() {
            publish(status);
        }
        shown.is_some()
    };
    match result {
        Ok(()) => {
            let shown = show(statuses.ready.clone());
            (Ok(()), owner.filter(|_| shown))
        }
        Err(asr::AsrError::Cancelled) => {
            // Abandoned for a cancelled recording, a model change or quit.
            show(residency_status(
                asr::ModelResidency::Absent,
                statuses.loading,
                statuses.detail,
            ));
            (Err(MODEL_LOAD_CANCELLED.into()), None)
        }
        Err(error) => {
            let message =
                crate::pipeline::model_load::model_load_failure_message(&error).to_owned();
            show(ModelStatus {
                model_id: statuses.loading.model_id.clone(),
                quantization: statuses.loading.quantization.clone(),
                installed: false,
                state: "error".into(),
                detail: message.clone(),
            });
            (Err(message), None)
        }
    }
}

/// The model status and the load that may change it, under one lock.
///
/// A load takes ownership when it starts and may write only while it still
/// owns the status, so a slow load never overwrites a newer load's status.
/// Statuses of two loads can be identical, so ownership is an id, not a
/// value. A settings save takes ownership away from every load.
pub(crate) struct ModelStatusSlot {
    inner: Mutex<OwnedModelStatus>,
}

struct OwnedModelStatus {
    status: ModelStatus,
    /// Id of the load that owns `status`; ids of settings saves own none.
    owner: u64,
    next_owner: u64,
}

impl ModelStatusSlot {
    pub(crate) fn new(status: ModelStatus) -> Self {
        Self {
            inner: Mutex::new(OwnedModelStatus {
                status,
                owner: 0,
                next_owner: 1,
            }),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, OwnedModelStatus>, String> {
        self.inner
            .lock()
            .map_err(|_| "model service is unavailable".to_string())
    }

    pub(crate) fn get(&self) -> Result<ModelStatus, String> {
        self.lock().map(|inner| inner.status.clone())
    }

    fn take_ownership(inner: &mut OwnedModelStatus, status: ModelStatus) -> u64 {
        let owner = inner.next_owner;
        inner.next_owner += 1;
        inner.owner = owner;
        inner.status = status;
        owner
    }

    /// Show `status` for a load that starts now; it owns the status from here.
    pub(crate) fn begin_load(&self, status: ModelStatus) -> Result<u64, String> {
        let mut inner = self.lock()?;
        Ok(Self::take_ownership(&mut inner, status))
    }

    /// Show `status` for a user action (a settings save); no load owns it.
    pub(crate) fn replace(&self, status: ModelStatus) -> Result<(), String> {
        let mut inner = self.lock()?;
        Self::take_ownership(&mut inner, status);
        Ok(())
    }

    /// Show `status` for `load`, only while it still owns the status.
    pub(crate) fn update(&self, load: u64, status: ModelStatus) -> Option<ModelStatus> {
        let mut inner = self.inner.lock().ok()?;
        (inner.owner == load).then(|| {
            inner.status = status.clone();
            status
        })
    }
}

fn same_model_status(left: &ModelStatus, right: &ModelStatus) -> bool {
    left.state == right.state
        && left.model_id == right.model_id
        && left.quantization == right.quantization
        && left.installed == right.installed
        && left.detail == right.detail
}

/// The status a loaded model has while the transcriber reports `residency`.
fn residency_status(
    residency: asr::ModelResidency,
    ready: &ModelStatus,
    detail: &str,
) -> ModelStatus {
    let (installed, state) = match residency {
        asr::ModelResidency::Resident(_) => return ready.clone(),
        asr::ModelResidency::Restoring => (false, "loading"),
        asr::ModelResidency::Absent => (false, "not_loaded"),
    };
    ModelStatus {
        model_id: ready.model_id.clone(),
        quantization: ready.quantization.clone(),
        installed,
        state: state.into(),
        detail: detail.to_owned(),
    }
}

/// Keep a loaded model's status true to the worker: not loaded once a reset
/// loses it (a crash, a timeout, a cancel the worker did not confirm), loading
/// with progress while a respawned worker restores it, and ready again after.
/// Stops once another load or a settings change takes over the status.
fn follow_model_residency(
    app: &AppHandle,
    services: &Services,
    load: u64,
    ready: &ModelStatus,
    detail: String,
) {
    let Some(mut residency) = services.transcriber.model_residency() else {
        return;
    };
    let app = app.clone();
    let ready = ready.clone();
    tauri::async_runtime::spawn(async move {
        let services = app.state::<Services>();
        let mut shown = ready.clone();
        let mut forwarder: Option<tauri::async_runtime::JoinHandle<()>> = None;
        // Checked at once: the model may already be gone.
        let mut current = *residency.borrow_and_update();
        loop {
            let next = residency_status(current, &ready, &detail);
            if !same_model_status(&next, &shown) {
                let Some(status) = services.model.update(load, next) else {
                    break;
                };
                if let Some(forwarder) = forwarder.take() {
                    forwarder.abort();
                }
                if current == asr::ModelResidency::Restoring {
                    emit_status(&app, "model_loading", "Loading the speech model.");
                    forwarder = spawn_load_progress_forwarder(&app, &services);
                }
                let _ = app.emit("model-status", status.clone());
                shown = status;
            }
            if residency.changed().await.is_err() {
                break;
            }
            current = *residency.borrow_and_update();
        }
        if let Some(forwarder) = forwarder {
            forwarder.abort();
        }
    });
}

#[cfg(test)]
mod model_status_tests {
    use super::*;

    #[test]
    fn prepared_model_must_match_the_selected_model_and_quantization() {
        let settings = Settings::default();
        let mut current = ModelStatus {
            model_id: model_identity(&settings).0,
            quantization: settings.model_quantization.clone(),
            installed: true,
            state: "ready".into(),
            detail: String::new(),
        };
        assert!(is_prepared_model(&current, &settings));
        current.quantization = "8bit".into();
        assert!(!is_prepared_model(&current, &settings));
        current.quantization = settings.model_quantization.clone();
        current.state = "loading".into();
        assert!(!is_prepared_model(&current, &settings));
    }

    fn status(state: &str, detail: &str) -> ModelStatus {
        ModelStatus {
            model_id: Some("model".into()),
            quantization: "4bit".into(),
            installed: state == "ready",
            state: state.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn a_slower_older_load_never_overwrites_a_newer_loads_status() {
        for (newer_outcome, older_outcome) in [
            ("ready", "ready"),
            ("ready", "error"),
            ("error", "ready"),
            ("error", "error"),
        ] {
            let slot = ModelStatusSlot::new(status("not_loaded", "d"));
            let older = slot.begin_load(status("loading", "d")).unwrap();
            let newer = slot.begin_load(status("loading", "d")).unwrap();

            // B finishes first, then A's outcome arrives late.
            assert!(slot.update(newer, status(newer_outcome, "B")).is_some());
            assert!(slot.update(older, status(older_outcome, "A")).is_none());
            let shown = slot.get().unwrap();
            assert_eq!(
                (shown.state.as_str(), shown.detail.as_str()),
                (newer_outcome, "B")
            );
        }
    }

    #[test]
    fn an_older_load_finishing_first_is_replaced_when_the_newer_one_ends() {
        let slot = ModelStatusSlot::new(status("not_loaded", "d"));
        let older = slot.begin_load(status("loading", "d")).unwrap();
        let newer = slot.begin_load(status("loading", "d")).unwrap();

        // Identical "loading" statuses: A's cancel must not touch B's download.
        assert!(slot.update(older, status("not_loaded", "A")).is_none());
        assert_eq!(slot.get().unwrap().state, "loading");
        assert!(slot.update(older, status("ready", "A")).is_none());
        assert!(slot.update(newer, status("error", "B")).is_some());
        assert_eq!(slot.get().unwrap().detail, "B");
    }

    #[test]
    fn a_settings_save_takes_the_status_from_every_load() {
        let slot = ModelStatusSlot::new(status("not_loaded", "d"));
        let load = slot.begin_load(status("loading", "d")).unwrap();
        slot.replace(status("not_loaded", "other model")).unwrap();

        assert!(slot.update(load, status("ready", "d")).is_none());
        assert_eq!(slot.get().unwrap().detail, "other model");
    }

    /// Loads run one at a time like the worker's: a load holds `worker`
    /// until `release` opens.
    struct QueuedTranscriber {
        worker: tokio::sync::Mutex<()>,
        release: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait::async_trait]
    impl Transcriber for QueuedTranscriber {
        async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
            unreachable!("loads go through load_when_started")
        }

        async fn load_when_started(
            &self,
            _: &str,
            cancel: Option<tokio::sync::watch::Receiver<bool>>,
            started: &(dyn Fn() + Send + Sync),
        ) -> Result<(), asr::AsrError> {
            let _worker = match cancel {
                Some(mut cancel) => tokio::select! {
                    worker = self.worker.lock() => worker,
                    _ = cancel.wait_for(|cancelled| *cancelled) => {
                        return Err(asr::AsrError::Cancelled);
                    }
                },
                None => self.worker.lock().await,
            };
            started();
            let mut release = self.release.clone();
            let _ = release.wait_for(|open| *open).await;
            Ok(())
        }

        async fn transcribe(
            &self,
            _: &Path,
            _: Option<&str>,
            _: tokio::sync::watch::Receiver<bool>,
        ) -> Result<asr::Transcript, asr::AsrError> {
            unreachable!("not used")
        }

        async fn shutdown(&self) -> Result<(), asr::AsrError> {
            Ok(())
        }

        async fn reconfigure(&self, _: asr::WorkerCommand) {}
    }

    fn spawn_load(
        slot: &Arc<ModelStatusSlot>,
        transcriber: &Arc<QueuedTranscriber>,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> tokio::task::JoinHandle<(Result<(), String>, Option<u64>)> {
        let slot = slot.clone();
        let transcriber = transcriber.clone();
        tokio::spawn(async move {
            let loading = status("loading", "d");
            let ready = status("ready", "d Loaded with 4bit quantization.");
            load_and_publish_status(
                &slot,
                transcriber.as_ref(),
                "4bit",
                cancel,
                ModelLoadStatuses {
                    loading: &loading,
                    ready: &ready,
                    detail: "d",
                },
                &|_| {},
            )
            .await
        })
    }

    #[tokio::test]
    async fn a_load_cancelled_while_waiting_leaves_the_running_load_its_status() {
        // The startup download (A) is running when a recording start (B)
        // needs the model; Esc cancels B before it ever reaches the worker.
        let slot = Arc::new(ModelStatusSlot::new(status("not_loaded", "d")));
        let (release_tx, release) = tokio::sync::watch::channel(false);
        let transcriber = Arc::new(QueuedTranscriber {
            worker: tokio::sync::Mutex::new(()),
            release,
        });
        let startup = spawn_load(&slot, &transcriber, None);
        while slot.get().unwrap().state != "loading" {
            tokio::task::yield_now().await;
        }

        let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
        let recording = spawn_load(&slot, &transcriber, Some(cancel));
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_tx.send_replace(true);
        let (result, shown_by) = recording.await.unwrap();
        assert_eq!(result, Err(MODEL_LOAD_CANCELLED.to_string()));
        assert_eq!(shown_by, None);
        assert_eq!(slot.get().unwrap().state, "loading");

        release_tx.send_replace(true);
        let (result, shown_by) = startup.await.unwrap();
        assert_eq!(result, Ok(()));
        assert!(shown_by.is_some());
        assert_eq!(slot.get().unwrap().state, "ready");
    }

    #[tokio::test]
    async fn a_load_that_reaches_the_worker_takes_the_status_over() {
        let slot = Arc::new(ModelStatusSlot::new(status("not_loaded", "d")));
        let (release_tx, release) = tokio::sync::watch::channel(true);
        let transcriber = Arc::new(QueuedTranscriber {
            worker: tokio::sync::Mutex::new(()),
            release,
        });
        let (result, first) = spawn_load(&slot, &transcriber, None).await.unwrap();
        assert_eq!(result, Ok(()));
        let (result, second) = spawn_load(&slot, &transcriber, None).await.unwrap();
        assert_eq!(result, Ok(()));
        assert!(first.is_some() && second.is_some() && first != second);
        // The first load no longer owns the status.
        assert!(slot
            .update(first.unwrap(), status("error", "late"))
            .is_none());
        drop(release_tx);
    }

    #[test]
    fn quantization_save_takes_status_away_from_an_old_load() {
        let slot = ModelStatusSlot::new(status("not_loaded", "d"));
        let old = slot.begin_load(status("loading", "d")).unwrap();
        let mut changed = status("not_loaded", "new quantization");
        changed.quantization = "8bit".into();
        slot.replace(changed.clone()).unwrap();
        assert!(slot
            .update(old, status("ready", "old quantization"))
            .is_none());
        assert!(same_model_status(&slot.get().unwrap(), &changed));
    }

    #[test]
    fn slots_do_not_share_ownership() {
        let first = ModelStatusSlot::new(status("not_loaded", "d"));
        let second = ModelStatusSlot::new(status("not_loaded", "d"));
        let load = first.begin_load(status("loading", "d")).unwrap();
        second.begin_load(status("loading", "d")).unwrap();
        second.begin_load(status("loading", "d")).unwrap();

        assert!(first.update(load, status("ready", "d")).is_some());
    }

    #[test]
    fn residency_maps_to_the_status_of_the_loaded_model() {
        let ready = status("ready", "d Loaded with 4bit quantization.");
        assert!(same_model_status(
            &residency_status(asr::ModelResidency::Resident(3), &ready, "d"),
            &ready
        ));
        let restoring = residency_status(asr::ModelResidency::Restoring, &ready, "d");
        assert_eq!(
            (restoring.state.as_str(), restoring.installed),
            ("loading", false)
        );
        assert_eq!(restoring.detail, "d");
        let absent = residency_status(asr::ModelResidency::Absent, &ready, "d");
        assert_eq!(
            (absent.state.as_str(), absent.installed),
            ("not_loaded", false)
        );
        assert_eq!(absent.model_id, ready.model_id);
    }
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
    let mut command = crate::environment::helper_command("nvidia-smi");
    command.args([
        "--query-gpu=name,driver_version,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    // The release app has no console for this console tool to share, so it
    // would otherwise open a window of its own.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output();
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

mod asr;
mod audio;
mod commands;
mod injection;
mod state;
mod storage;
mod types;

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use asr::{JsonlTranscriber, Transcriber, WorkerCommand};
use audio::{AudioCapture, AudioDevice, CaptureConfig, CpalAudioCapture};
use injection::{InjectionOptions, InsertResult, SystemTextInjector, TargetWindow, TextInjector};
use state::{AppState, PipelineLifecycle, PipelinePhase};
use storage::Storage;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, State,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use types::{
    AppPhase, AppStateSnapshot, DictionaryEntry, DictionaryEntryInput, GpuDiagnostics, HistoryItem,
    LoadModelRequest, ModelStatus, NewDictionaryEntry, NewHistoryItem, RecordingResult, Settings,
};

pub(crate) fn model_identity(asr_backend: &str) -> (Option<String>, String) {
    match asr_backend {
        "faster-whisper" => (
            Some("faster-whisper:large-v3-turbo".into()),
            "faster-whisper runs locally on CPU or GPU; the model downloads on first load.".into(),
        ),
        "vibevoice" => (
            Some("microsoft/VibeVoice-ASR-HF".into()),
            "VibeVoice requires a CUDA GPU; downloads from Hugging Face on first load.".into(),
        ),
        "mock" => (
            Some("mock".into()),
            "Mock backend for development; no model is downloaded.".into(),
        ),
        _ => (None, format!("Unknown ASR backend: {asr_backend}.")),
    }
}

pub(crate) struct Services {
    audio: Mutex<Option<Box<dyn AudioCapture>>>,
    target: Mutex<Option<TargetWindow>>,
    transcriber: Arc<dyn Transcriber>,
    model: Mutex<ModelStatus>,
    lifecycle: PipelineLifecycle,
}

impl Services {
    fn new(asr_backend: &str) -> Self {
        let (model_id, detail) = model_identity(asr_backend);
        Self {
            audio: Mutex::new(Some(Box::new(CpalAudioCapture::new()))),
            target: Mutex::new(None),
            transcriber: Arc::new(JsonlTranscriber::new(
                worker_command_for_backend(asr_backend),
                Duration::from_secs(300),
            )),
            model: Mutex::new(ModelStatus {
                model_id,
                installed: false,
                state: "not_loaded".into(),
                detail,
            }),
            lifecycle: PipelineLifecycle::default(),
        }
    }
}

pub(crate) struct PipelineGuard<'a> {
    lifecycle: &'a PipelineLifecycle,
    id: u64,
}

impl Drop for PipelineGuard<'_> {
    fn drop(&mut self) {
        self.lifecycle.finish(self.id);
    }
}

pub(crate) struct TempArtifact {
    path: PathBuf,
    retain: bool,
}

impl TempArtifact {
    fn new(path: PathBuf, retain: bool) -> Self {
        Self { path, retain }
    }

    fn cleanup(&mut self) -> io::Result<()> {
        if self.retain {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        if !self.retain {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn command_error(error: impl std::fmt::Display) -> String {
    format!("local operation failed: {error}")
}

pub(crate) fn worker_command_for_backend(asr_backend: &str) -> WorkerCommand {
    let python = std::env::var_os("ASR_PYTHON").unwrap_or_else(|| "python".into());
    WorkerCommand::python(python).with_backend(asr_backend)
}

pub(crate) fn emit_state(app: &AppHandle, state: &AppState, phase: AppPhase, message: &str) {
    let snapshot = state.transition(phase, Some(message.into()));
    let _ = app.emit("app-state", snapshot);
}

pub(crate) fn emit_status(app: &AppHandle, kind: &str, message: &str) {
    let _ = app.emit(
        "status",
        serde_json::json!({ "kind": kind, "message": message }),
    );
}

pub(crate) fn take_audio(services: &Services) -> Result<Box<dyn AudioCapture>, String> {
    services
        .audio
        .lock()
        .map_err(|_| "audio service is unavailable".to_string())?
        .take()
        .ok_or_else(|| "audio operation is already in progress".to_string())
}

pub(crate) fn return_audio(services: &Services, audio: Box<dyn AudioCapture>) {
    if let Ok(mut slot) = services.audio.lock() {
        *slot = Some(audio);
    }
}

fn database_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let directory = app.path().app_data_dir()?;
    fs::create_dir_all(&directory)?;
    Ok(directory.join("local-voice-input.sqlite3"))
}

fn parse_shortcut(value: &str) -> Result<Shortcut, String> {
    Shortcut::from_str(value.trim()).map_err(|_| "hotkey is invalid".to_string())
}

fn cleanup_stale_artifacts(directory: &Path) -> io::Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("local-ai-voice-")
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "wav")
        {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

async fn toggle_recording(app: AppHandle) {
    let phase = app.state::<AppState>().snapshot().phase;
    let result = if phase == AppPhase::Recording {
        commands::stop_recording(
            app.clone(),
            app.state::<Services>(),
            app.state::<AppState>(),
            app.state::<Storage>(),
        )
        .await
        .map(|_| ())
    } else {
        commands::start_recording(
            app.clone(),
            app.state::<Services>(),
            app.state::<AppState>(),
            app.state::<Storage>(),
        )
        .await
    };
    if let Err(error) = result {
        emit_status(&app, "error", &error);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, _pressed, event| {
                    if event.state() == ShortcutState::Pressed {
                        let app = app.clone();
                        tauri::async_runtime::spawn(toggle_recording(app));
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let storage = Storage::open(&database_path(app.handle())?)?;
            storage.enforce_current_history_policy()?;
            let settings = storage.get_settings()?;
            let autostart_result = if settings.auto_start {
                app.autolaunch().enable()
            } else {
                app.autolaunch().disable()
            };
            if let Err(error) = autostart_result {
                emit_status(
                    app.handle(),
                    "autostart_update_failed",
                    &format!("Autostart update failed: {error}"),
                );
            }
            let shortcut = parse_shortcut(&settings.hotkey)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            app.manage(storage);
            app.manage(AppState::default());
            app.manage(Services::new(&settings.asr_backend));
            app.global_shortcut().register(shortcut)?;
            if cleanup_stale_artifacts(&std::env::temp_dir()).is_err() {
                emit_status(
                    app.handle(),
                    "artifact_cleanup_failed",
                    "Stale temporary audio cleanup failed.",
                );
            }

            let show =
                MenuItem::with_id(app, "show", "Open Local Voice Input", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Local Voice Input")
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_audio_devices,
            commands::start_recording,
            commands::stop_recording,
            commands::cancel_recording,
            commands::get_app_state,
            commands::get_settings,
            commands::get_model_status,
            commands::load_model,
            commands::run_gpu_diagnostics,
            commands::list_history,
            commands::list_dictionary,
            commands::add_dictionary_entry,
            commands::delete_dictionary_entry,
            commands::copy_history_item,
            commands::update_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running Local Voice Input");
}

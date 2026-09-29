mod asr;
mod audio;
mod commands;
mod correction;
mod correction_prompt;
mod injection;
mod input_monitor;
mod live_dictation;
mod recording_overlay;
mod state;
mod storage;
mod types;

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use asr::{JsonlTranscriber, Transcriber, WorkerCommand};
use audio::{
    AudioCapture, AudioDevice, AudioEnhancementConfig, CaptureConfig, CpalAudioCapture,
    NoiseSuppressionLevel,
};
use injection::{InjectionOptions, InsertResult, SystemTextInjector, TargetWindow, TextInjector};
use input_monitor::InputMonitor;
use state::{AppState, PipelineLifecycle, PipelineMode, PipelinePhase};
use storage::Storage;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, RunEvent, State, WindowEvent,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use types::{
    AppPhase, AppStateSnapshot, DictionaryEntry, DictionaryEntryInput, GpuDiagnostics, HistoryItem,
    LoadModelRequest, ModelStatus, NewDictionaryEntry, NewHistoryItem, RecordingResult, Settings,
};

const ASR_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const ASR_MODEL_LOAD_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

pub(crate) fn model_identity(settings: &Settings) -> (Option<String>, String) {
    let custom_model = settings
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match settings.asr_backend.as_str() {
        "faster-whisper" => (
            Some(format!(
                "faster-whisper:{}",
                custom_model.unwrap_or("large-v3-turbo")
            )),
            "faster-whisper runs locally on CPU or GPU. Model names and compatible Hugging Face CTranslate2 repositories download on first load.".into(),
        ),
        "vibevoice" => (
            Some(custom_model.unwrap_or("microsoft/VibeVoice-ASR-HF").into()),
            "VibeVoice requires a CUDA GPU; downloads from Hugging Face on first load.".into(),
        ),
        "openai-compatible" => (
            Some(format!(
                "openai-compatible:{}",
                custom_model.unwrap_or("gpt-4o-mini-transcribe")
            )),
            format!(
                "Uses an OpenAI-compatible Audio Transcriptions API at {}. The API key is read from the {} environment variable.",
                settings.api_base_url, settings.api_key_env_var
            ),
        ),
        "mock" => (
            Some("mock".into()),
            "Mock backend for development; no model is downloaded.".into(),
        ),
        _ => (None, format!("Unknown ASR backend: {}.", settings.asr_backend)),
    }
}

pub(crate) struct Services {
    audio: tokio::sync::Mutex<Box<dyn AudioCapture>>,
    live: tokio::sync::Mutex<Option<live_dictation::LiveTask>>,
    target: Mutex<Option<TargetWindow>>,
    transcriber: Arc<dyn Transcriber>,
    input_monitor: Arc<InputMonitor>,
    model: Mutex<ModelStatus>,
    gpu_diagnostics: Mutex<Option<GpuDiagnostics>>,
    lifecycle: PipelineLifecycle,
    initialization: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    shutdown_started: AtomicBool,
    translation_active: AtomicBool,
    voice_translation_target: Mutex<Option<String>>,
    // Set at startup and replaced by every successful settings save.
    startup_hotkey_issues: Mutex<HotkeyIssues>,
    registered_hotkeys: Mutex<Vec<Shortcut>>,
}

/// Global shortcut actions. `hotkey_bindings` lists them in collision
/// precedence order: when saved chords collide, the older action keeps the
/// chord and a later one is not dispatched on it until it is reassigned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HotkeyAction {
    Dictate,
    SelectedTextTranslate,
    VoiceTranslate,
}

impl HotkeyAction {
    fn registration_failed_message(self) -> &'static str {
        match self {
            Self::Dictate => "The recording hotkey could not be registered; another app may be using it. Change it in Settings.",
            Self::SelectedTextTranslate => "The selected-text translation hotkey could not be registered; another app may be using it. Change it in Settings.",
            Self::VoiceTranslate => "The voice Translate hotkey could not be registered; another app may be using it. Change it in Settings.",
        }
    }

    fn collision_message(self, owner: Self) -> &'static str {
        match (self, owner) {
            (Self::SelectedTextTranslate, Self::Dictate) => "The selected-text translation hotkey matches the recording hotkey, so selected-text translation is off until you change it in Settings.",
            (Self::VoiceTranslate, Self::Dictate) => "The voice Translate hotkey matches the recording hotkey, so voice Translate is off until you change it in Settings.",
            (Self::VoiceTranslate, Self::SelectedTextTranslate) => "The voice Translate hotkey matches the selected-text translation hotkey, so voice Translate is off until you change it in Settings.",
            _ => "Some saved hotkeys overlap. Change them in Settings to enable each action independently.",
        }
    }
}

type HotkeyBinding = (HotkeyAction, Shortcut);

/// The saved chords in collision precedence order.
fn hotkey_bindings(settings: &Settings) -> Result<Vec<HotkeyBinding>, String> {
    Ok(vec![
        (HotkeyAction::Dictate, parse_shortcut(&settings.hotkey)?),
        (
            HotkeyAction::SelectedTextTranslate,
            parse_shortcut(&settings.translation_hotkey)?,
        ),
        (
            HotkeyAction::VoiceTranslate,
            parse_shortcut(&settings.voice_translate_hotkey)?,
        ),
    ])
}

#[derive(Debug, Default, PartialEq, Eq)]
struct HotkeyAssignment {
    /// One binding per distinct chord, owned by the first action claiming it.
    active: Vec<HotkeyBinding>,
    /// `(unassigned action, action that keeps the chord)` pairs.
    collisions: Vec<(HotkeyAction, HotkeyAction)>,
}

fn assign_hotkeys(bindings: &[HotkeyBinding]) -> HotkeyAssignment {
    let mut assignment = HotkeyAssignment::default();
    for &(action, shortcut) in bindings {
        match assignment
            .active
            .iter()
            .find(|(_, chord)| *chord == shortcut)
        {
            Some(&(owner, _)) => assignment.collisions.push((action, owner)),
            None => assignment.active.push((action, shortcut)),
        }
    }
    assignment
}

/// The action a pressed chord runs. Only the chord's owner is dispatched, so
/// an action that lost a collision never runs on that chord.
fn dispatched_action(bindings: &[HotkeyBinding], pressed: Shortcut) -> Option<HotkeyAction> {
    assign_hotkeys(bindings)
        .active
        .into_iter()
        .find(|(_, chord)| *chord == pressed)
        .map(|(action, _)| action)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct HotkeyIssues {
    collisions: Vec<(HotkeyAction, HotkeyAction)>,
    unregistered: Vec<HotkeyAction>,
}

impl HotkeyIssues {
    /// One fixed, localizable sentence per affected action.
    fn messages(&self) -> Vec<&'static str> {
        self.collisions
            .iter()
            .map(|&(action, owner)| action.collision_message(owner))
            .chain(
                self.unregistered
                    .iter()
                    .map(|action| action.registration_failed_message()),
            )
            .collect()
    }
}

impl Services {
    fn new(settings: &Settings) -> Self {
        let (model_id, detail) = model_identity(settings);
        Self {
            audio: tokio::sync::Mutex::new(Box::new(CpalAudioCapture::new())),
            live: tokio::sync::Mutex::new(None),
            target: Mutex::new(None),
            transcriber: Arc::new(JsonlTranscriber::new(
                worker_command_for_settings(settings),
                ASR_REQUEST_TIMEOUT,
                ASR_MODEL_LOAD_TIMEOUT,
            )),
            input_monitor: Arc::new(InputMonitor::default()),
            model: Mutex::new(ModelStatus {
                model_id,
                installed: false,
                state: "not_loaded".into(),
                detail,
            }),
            gpu_diagnostics: Mutex::new(None),
            lifecycle: PipelineLifecycle::default(),
            initialization: Mutex::new(None),
            shutdown_started: AtomicBool::new(false),
            translation_active: AtomicBool::new(false),
            voice_translation_target: Mutex::new(None),
            startup_hotkey_issues: Mutex::new(HotkeyIssues::default()),
            registered_hotkeys: Mutex::new(Vec::new()),
        }
    }

    fn begin_shutdown(&self) -> bool {
        self.shutdown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    async fn shutdown(&self) {
        let _ = self.lifecycle.cancel();
        if let Ok(mut initialization) = self.initialization.lock() {
            if let Some(task) = initialization.take() {
                task.abort();
            }
        }
        {
            let mut audio = self.audio.lock().await;
            let _ = audio.cancel().await;
        }
        if let Some(task) = self.live.lock().await.take() {
            let _ = task.finish().await;
        }
        if let Ok(mut target) = self.target.lock() {
            target.take();
        }
        if let Ok(mut language) = self.voice_translation_target.lock() {
            language.take();
        }
        self.input_monitor.shutdown();
        let _ = self.transcriber.shutdown().await;
    }
}

pub fn run_input_monitor_worker() {
    input_monitor::run_worker();
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

pub(crate) fn worker_command_for_settings(settings: &Settings) -> WorkerCommand {
    let (python, project_root) = worker_runtime();
    let mut command = WorkerCommand::python(python).with_backend(&settings.asr_backend);
    if let Some(model_id) = settings
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        command.env.push(("ASR_MODEL_ID".into(), model_id.into()));
    }
    if settings.asr_backend == "openai-compatible" {
        command.env.push((
            "ASR_API_BASE_URL".into(),
            settings.api_base_url.trim().into(),
        ));
        if let Ok(api_key) = std::env::var(settings.api_key_env_var.trim()) {
            command.env.push(("ASR_API_KEY".into(), api_key));
        }
    }

    // The desktop app is often launched from a shortcut, so Python does not
    // necessarily inherit the repository directory as its working directory.
    // Keep the source worker importable in development and in local builds.
    if let Some(root) = project_root {
        let root = root.to_string_lossy().into_owned();
        let python_path = std::env::var_os("PYTHONPATH")
            .map(|value| {
                let separator = if cfg!(windows) { ";" } else { ":" };
                format!("{root}{separator}{}", value.to_string_lossy())
            })
            .unwrap_or(root);
        command.env.push(("PYTHONPATH".into(), python_path));
    }
    command
}

fn worker_runtime() -> (OsString, Option<PathBuf>) {
    let roots = [
        std::env::current_dir().ok(),
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf)),
    ];
    let mut project_root = None;
    for root in roots.into_iter().flatten() {
        for candidate in root.ancestors() {
            if candidate.join("asr_worker").join("__main__.py").is_file() {
                project_root = Some(candidate.to_path_buf());
                break;
            }
        }
        if project_root.is_some() {
            break;
        }
    }

    if let Some(python) = std::env::var_os("ASR_PYTHON") {
        return (python, project_root);
    }
    if let Some(root) = project_root.as_ref() {
        let venv_python = if cfg!(windows) {
            root.join(".venv").join("Scripts").join("python.exe")
        } else {
            root.join(".venv").join("bin").join("python")
        };
        if venv_python.is_file() {
            return (venv_python.into_os_string(), project_root);
        }
    }
    ("python".into(), project_root)
}

pub(crate) fn emit_state(app: &AppHandle, state: &AppState, phase: AppPhase, message: &str) {
    recording_overlay::set_phase(app, &phase);
    let snapshot = state.transition(phase, Some(message.into()));
    let _ = app.emit("app-state", snapshot);
}

pub(crate) fn emit_status(app: &AppHandle, kind: &str, message: &str) {
    let _ = app.emit(
        "status",
        serde_json::json!({ "kind": kind, "message": message }),
    );
}

fn database_path(app: &AppHandle) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let directory = app.path().app_data_dir()?;
    fs::create_dir_all(&directory)?;
    Ok(directory.join("local-voice-input.sqlite3"))
}

fn parse_shortcut(value: &str) -> Result<Shortcut, String> {
    Shortcut::from_str(value.trim()).map_err(|_| "hotkey is invalid".to_string())
}

#[cfg(test)]
mod shortcut_startup_tests {
    use super::*;
    use HotkeyAction::{Dictate, SelectedTextTranslate, VoiceTranslate};

    fn bindings(dictate: &str, selected: &str, voice: &str) -> Vec<HotkeyBinding> {
        let mut settings = Settings::default();
        settings.hotkey = dictate.into();
        settings.translation_hotkey = selected.into();
        settings.voice_translate_hotkey = voice.into();
        hotkey_bindings(&settings).unwrap()
    }

    #[test]
    fn legacy_selected_text_chord_keeps_precedence_over_voice_translate() {
        // A legacy selected-text chord equal to the new voice Translate default.
        let mut settings = Settings::default();
        settings.translation_hotkey = settings.voice_translate_hotkey.clone();
        let bindings = hotkey_bindings(&settings).unwrap();
        let recording = parse_shortcut(&settings.hotkey).unwrap();
        let shared = parse_shortcut(&settings.translation_hotkey).unwrap();

        let assignment = assign_hotkeys(&bindings);
        assert_eq!(
            assignment.active,
            vec![(Dictate, recording), (SelectedTextTranslate, shared)]
        );
        assert_eq!(
            assignment.collisions,
            vec![(VoiceTranslate, SelectedTextTranslate)]
        );
        assert_eq!(
            dispatched_action(&bindings, shared),
            Some(SelectedTextTranslate)
        );
        assert_eq!(dispatched_action(&bindings, recording), Some(Dictate));
    }

    #[test]
    fn dictate_keeps_a_chord_shared_with_every_other_action() {
        let bindings = bindings("Ctrl+Shift+Space", "Ctrl+Shift+Space", "Ctrl+Shift+Space");
        let shared = parse_shortcut("Ctrl+Shift+Space").unwrap();
        let assignment = assign_hotkeys(&bindings);
        assert_eq!(assignment.active, vec![(Dictate, shared)]);
        assert_eq!(
            assignment.collisions,
            vec![(SelectedTextTranslate, Dictate), (VoiceTranslate, Dictate)]
        );
        assert_eq!(dispatched_action(&bindings, shared), Some(Dictate));
    }

    #[test]
    fn distinct_and_unassigned_chords_dispatch_their_own_action() {
        let bindings = bindings("Ctrl+Shift+Space", "Ctrl+Shift+Space", "Ctrl+Shift+Y");
        assert_eq!(
            assign_hotkeys(&bindings).collisions,
            vec![(SelectedTextTranslate, Dictate)]
        );
        assert_eq!(
            dispatched_action(&bindings, parse_shortcut("Ctrl+Shift+Y").unwrap()),
            Some(VoiceTranslate)
        );
        assert_eq!(
            dispatched_action(&bindings, parse_shortcut("Ctrl+Shift+U").unwrap()),
            None
        );
    }

    #[test]
    fn hotkey_warnings_name_each_affected_action() {
        assert!(HotkeyIssues::default().messages().is_empty());
        let issues = HotkeyIssues {
            collisions: vec![
                (SelectedTextTranslate, Dictate),
                (VoiceTranslate, SelectedTextTranslate),
            ],
            unregistered: vec![Dictate],
        };
        let messages = issues.messages();
        assert_eq!(messages.len(), 3);
        assert!(messages[0]
            .starts_with("The selected-text translation hotkey matches the recording hotkey"));
        assert!(messages[1].starts_with(
            "The voice Translate hotkey matches the selected-text translation hotkey"
        ));
        assert!(messages[1].contains("voice Translate is off"));
        assert!(messages[2].starts_with("The recording hotkey could not be registered"));
    }
}

fn load_environment_file() {
    let current_dir = std::env::current_dir().ok();
    let executable = std::env::current_exe().ok();
    let candidates = environment_file_candidates(current_dir.as_deref(), executable.as_deref());

    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        if let Err(error) = dotenvy::from_path(&path) {
            eprintln!(
                "Failed to load environment file {}: {error}",
                path.display()
            );
        }
    }
}

fn environment_file_candidates(
    current_dir: Option<&Path>,
    executable: Option<&Path>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(current_dir) = current_dir {
        candidates.push(current_dir.join(".env"));
        if current_dir
            .file_name()
            .is_some_and(|name| name == "src-tauri")
        {
            if let Some(workspace) = current_dir.parent() {
                candidates.push(workspace.join(".env"));
            }
        }
    }
    if let Some(parent) = executable.and_then(Path::parent) {
        let path = parent.join(".env");
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

#[cfg(test)]
mod model_configuration_tests {
    use super::*;

    #[test]
    fn development_launch_finds_workspace_environment_file() {
        let workspace = Path::new("workspace");
        let current_dir = workspace.join("src-tauri");
        let executable = current_dir.join("target/debug/local-voice-input.exe");

        let candidates = environment_file_candidates(Some(&current_dir), Some(&executable));

        assert!(candidates.contains(&workspace.join(".env")));
    }

    #[test]
    fn custom_model_is_reflected_in_identity_and_worker_environment() {
        let settings = Settings {
            model_id: Some("openai/whisper-custom".into()),
            ..Settings::default()
        };

        let (identity, _) = model_identity(&settings);
        assert_eq!(
            identity.as_deref(),
            Some("faster-whisper:openai/whisper-custom")
        );

        let command = worker_command_for_settings(&settings);
        assert!(command
            .env
            .iter()
            .any(|(key, value)| key == "ASR_MODEL_ID" && value == "openai/whisper-custom"));
    }

    #[test]
    fn default_model_does_not_override_worker_environment() {
        let command = worker_command_for_settings(&Settings::default());
        assert!(!command.env.iter().any(|(key, _)| key == "ASR_MODEL_ID"));
    }

    #[test]
    fn api_backend_passes_endpoint_without_persisting_a_secret() {
        let settings = Settings {
            asr_backend: "openai-compatible".into(),
            api_base_url: "http://127.0.0.1:8000/v1".into(),
            ..Settings::default()
        };
        let command = worker_command_for_settings(&settings);
        assert!(command.env.iter().any(|(key, value)| {
            key == "ASR_API_BASE_URL" && value == "http://127.0.0.1:8000/v1"
        }));
    }

    #[test]
    fn stale_artifact_cleanup_preserves_live_and_unrecognized_files() {
        let directory = tempfile::tempdir().unwrap();
        let live = directory
            .path()
            .join(format!("local-ai-voice-{}-1-0.wav", std::process::id()));
        let dead = directory
            .path()
            .join(format!("local-ai-voice-{}-1-0.wav", u32::MAX));
        let unrecognized = directory
            .path()
            .join(format!("local-ai-voice-{}-1.wav", u32::MAX));
        fs::write(&live, b"live").unwrap();
        fs::write(&dead, b"dead").unwrap();
        fs::write(&unrecognized, b"unrecognized").unwrap();

        assert_eq!(cleanup_stale_artifacts(directory.path(), true).unwrap(), 1);
        assert!(live.exists());
        assert!(!dead.exists());
        assert!(unrecognized.exists());
    }

    #[test]
    fn stale_artifact_cleanup_is_skipped_when_audio_is_retained() {
        let directory = tempfile::tempdir().unwrap();
        let retained = directory
            .path()
            .join(format!("local-ai-voice-{}-1-0.wav", u32::MAX));
        fs::write(&retained, b"retained").unwrap();

        assert_eq!(
            cleanup_stale_artifacts(&directory.path().join("missing"), false).unwrap(),
            0
        );
        assert_eq!(cleanup_stale_artifacts(directory.path(), false).unwrap(), 0);
        assert!(retained.exists());
    }
}

fn artifact_process_id(name: &std::ffi::OsStr) -> Option<u32> {
    let components = name
        .to_str()?
        .strip_prefix("local-ai-voice-")?
        .strip_suffix(".wav")?
        .split('-')
        .collect::<Vec<_>>();
    if components.len() != 3 {
        return None;
    }
    let process_id = components[0].parse().ok()?;
    components[1].parse::<u128>().ok()?;
    components[2].parse::<u64>().ok()?;
    Some(process_id)
}

#[cfg(target_os = "windows")]
fn process_is_live(process_id: u32) -> bool {
    use windows::Win32::{
        Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE},
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    let process = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
    {
        Ok(process) => process,
        Err(error)
            if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) =>
        {
            return false;
        }
        // Access can be denied for protected processes. If liveness cannot be
        // disproved, preserve the artifact rather than risking another process's file.
        Err(_) => return true,
    };
    let mut exit_code = 0;
    let result = unsafe { GetExitCodeProcess(process, &mut exit_code) };
    let _ = unsafe { CloseHandle(process) };
    result.is_err() || exit_code == STILL_ACTIVE.0 as u32
}

#[cfg(not(target_os = "windows"))]
fn process_is_live(process_id: u32) -> bool {
    process_id == std::process::id() || Path::new("/proc").join(process_id.to_string()).exists()
}

fn cleanup_stale_artifacts(
    directory: &Path,
    delete_audio_after_processing: bool,
) -> io::Result<usize> {
    if !delete_audio_after_processing {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if artifact_process_id(&entry.file_name())
            .is_some_and(|process_id| !process_is_live(process_id))
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

async fn toggle_voice_mode(app: AppHandle, requested_mode: PipelineMode) {
    let phase = app.state::<Services>().lifecycle.phase();
    let result = match phase {
        PipelinePhase::Recording
            if app.state::<Services>().lifecycle.mode() == Some(requested_mode) =>
        {
            commands::stop_recording(
                app.clone(),
                app.state::<Services>(),
                app.state::<AppState>(),
                app.state::<Storage>(),
            )
            .await
            .map(|_| ())
        }
        PipelinePhase::Recording => Err("another voice mode is already recording".into()),
        PipelinePhase::Idle => match requested_mode {
            PipelineMode::Dictate => {
                commands::start_recording_with_origin(
                    app.clone(),
                    app.state::<Services>(),
                    app.state::<AppState>(),
                    app.state::<Storage>(),
                    true,
                )
                .await
            }
            PipelineMode::Translate => {
                commands::start_voice_translation_with_origin(
                    app.clone(),
                    app.state::<Services>(),
                    app.state::<AppState>(),
                    app.state::<Storage>(),
                    true,
                )
                .await
            }
        },
        PipelinePhase::Starting | PipelinePhase::Processing => Ok(()),
    };
    if let Err(error) = result {
        emit_status(&app, "error", &error);
    }
}

async fn translate_selection(app: AppHandle) {
    let services = app.state::<Services>();
    if services.lifecycle.phase() != PipelinePhase::Idle {
        return;
    }
    let active = &services.translation_active;
    if active.swap(true, Ordering::AcqRel) {
        return;
    }
    struct TranslationGuard<'a>(&'a AtomicBool);
    impl Drop for TranslationGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _guard = TranslationGuard(active);
    if services.lifecycle.phase() != PipelinePhase::Idle {
        return;
    }
    let settings = match app.state::<Storage>().get_settings() {
        Ok(settings) => settings,
        Err(_) => {
            emit_status(&app, "error", "Translation settings are unavailable.");
            return;
        }
    };
    let injector = SystemTextInjector::new(InjectionOptions {
        restore_clipboard: settings.clipboard_restore,
    });
    let selection = match injector.capture_selection() {
        Ok(selection) => selection,
        Err(_) => {
            emit_status(
                &app,
                "error",
                "Select text in a supported foreground edit control.",
            );
            return;
        }
    };
    let source = selection.text().to_owned();
    let monitor = Arc::clone(&app.state::<Services>().input_monitor);
    if !monitor.start() {
        emit_status(
            &app,
            "error",
            "Translation could not monitor the target safely.",
        );
        return;
    }
    let Some(checkpoint) = monitor.checkpoint() else {
        emit_status(
            &app,
            "error",
            "Translation could not monitor the target safely.",
        );
        return;
    };
    let (_cancel_guard, cancel) = tokio::sync::watch::channel(false);
    let translated = correction::translate_text(&settings, &source, cancel).await;
    let translated = match translated {
        Ok(text) => text,
        Err(_) => {
            let copied = injector.copy_to_clipboard(&source).is_ok();
            let message = if copied {
                "Translation failed; the selected text remains on the clipboard."
            } else {
                "Translation failed and the clipboard is unavailable."
            };
            emit_status(&app, "error", message);
            return;
        }
    };
    match injector.replace_selection(&selection, &translated, &monitor, checkpoint) {
        Ok(InsertResult::ClipboardOnly | InsertResult::PasteUnverified) => {
            emit_status(
                &app,
                "error",
                "The target changed; the translation remains on the clipboard.",
            );
        }
        Err(_) => {
            let copied = injector.copy_to_clipboard(&translated).is_ok();
            let message = if copied {
                "The target changed; the translation remains on the clipboard."
            } else {
                "The target changed and the clipboard is unavailable."
            };
            emit_status(&app, "error", message);
        }
        Ok(InsertResult::ClipboardPaste) => emit_status(&app, "success", "Translation inserted."),
    }
}

fn handle_shortcut(app: AppHandle, shortcut: Shortcut) {
    let Ok(settings) = app.state::<Storage>().get_settings() else {
        return;
    };
    let Ok(bindings) = hotkey_bindings(&settings) else {
        return;
    };
    match dispatched_action(&bindings, shortcut) {
        Some(HotkeyAction::Dictate) => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Dictate));
        }
        Some(HotkeyAction::SelectedTextTranslate) => {
            tauri::async_runtime::spawn(translate_selection(app));
        }
        Some(HotkeyAction::VoiceTranslate) => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Translate));
        }
        None => {}
    }
}

async fn initialize_model_runtime(app: AppHandle, settings: Settings) {
    let diagnostics = tauri::async_runtime::spawn_blocking(commands::probe_gpu_diagnostics)
        .await
        .unwrap_or_else(|_| GpuDiagnostics {
            status: "unavailable".into(),
            adapter_name: None,
            driver_version: None,
            memory_total_mb: None,
            recommendation: "GPU diagnostics could not be completed.".into(),
        });
    if let Ok(mut current) = app.state::<Services>().gpu_diagnostics.lock() {
        *current = Some(diagnostics.clone());
    }
    let diagnostic_kind = if diagnostics.status == "available" {
        "gpu_available"
    } else {
        "gpu_unavailable"
    };
    emit_status(&app, diagnostic_kind, &diagnostics.recommendation);
    let _ = app.emit("gpu-diagnostics", diagnostics);

    let services = app.state::<Services>();
    if let Err(error) = commands::ensure_model_loaded(&app, &services, &settings).await {
        emit_status(&app, "model_load_failed", &error);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    load_environment_file();
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, pressed, event| {
                    if event.state() == ShortcutState::Pressed {
                        handle_shortcut(app.clone(), *pressed);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let storage = Storage::open(&database_path(app.handle())?)?;
            storage.enforce_current_history_policy()?;
            let settings = storage.get_settings()?;
            // The autostart plugin registers the currently running executable.
            // A development executable depends on Vite's dev server and cannot
            // run on its own at Windows sign-in, so it must never replace the
            // registration created by an installed/release build.
            #[cfg(not(debug_assertions))]
            {
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
            }
            let bindings = hotkey_bindings(&settings)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            app.manage(storage);
            app.manage(AppState::default());
            app.manage(Services::new(&settings));
            recording_overlay::create(app.handle())?;
            // Older settings may share a chord with a newly added action. The
            // older action keeps it (the same owner dispatch uses); each chord
            // is registered once, and the warning names every unassigned action.
            let assignment = assign_hotkeys(&bindings);
            let mut issues = HotkeyIssues {
                collisions: assignment.collisions,
                unregistered: Vec::new(),
            };
            let mut registered = Vec::new();
            for (action, shortcut) in assignment.active {
                if app.global_shortcut().register(shortcut).is_ok() {
                    registered.push(shortcut);
                } else {
                    issues.unregistered.push(action);
                }
            }
            if let Ok(mut active) = app.state::<Services>().registered_hotkeys.lock() {
                *active = registered;
            }
            if let Ok(mut current) = app.state::<Services>().startup_hotkey_issues.lock() {
                *current = issues;
            }
            if cleanup_stale_artifacts(
                &std::env::temp_dir(),
                settings.delete_audio_after_processing,
            )
            .is_err()
            {
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
            let app_handle = app.handle().clone();
            let initialization =
                tauri::async_runtime::spawn(initialize_model_runtime(app_handle, settings));
            if let Ok(mut task) = app.state::<Services>().initialization.lock() {
                *task = Some(initialization);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_audio_devices,
            commands::start_recording,
            commands::start_voice_translation,
            commands::stop_recording,
            commands::cancel_recording,
            commands::cycle_voice_translation_target,
            commands::get_app_state,
            commands::get_startup_hotkey_warning,
            commands::get_settings,
            commands::get_model_status,
            commands::load_model,
            commands::get_gpu_diagnostics,
            commands::run_gpu_diagnostics,
            commands::list_history,
            commands::list_dictionary,
            commands::add_dictionary_entry,
            commands::delete_dictionary_entry,
            commands::copy_history_item,
            commands::copy_to_clipboard,
            commands::update_settings
        ])
        .on_window_event(|window, event| {
            if window.label() == "main" && matches!(event, WindowEvent::CloseRequested { .. }) {
                // A tray icon keeps a Tauri process alive after its last window
                // is closed. Treat the main window's close button as an actual
                // application exit, but keep the window alive until RunEvent's
                // asynchronous shutdown has stopped capture and the ASR worker.
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                }
                window.app_handle().exit(0);
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Local Voice Input")
        .run(|app, event| {
            if let RunEvent::ExitRequested { api, code, .. } = event {
                let services = app.state::<Services>();
                if services.begin_shutdown() {
                    api.prevent_exit();
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        app.state::<Services>().shutdown().await;
                        app.exit(code.unwrap_or(0));
                    });
                }
            }
        });
}

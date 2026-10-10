mod answer_panel;
mod app_context;
mod ask;
mod asr;
mod audio;
mod commands;
mod correction;
mod correction_prompt;
mod injection;
mod input_monitor;
mod live_dictation;
mod personalization;
mod recording_overlay;
mod shortcuts;
mod state;
mod storage;
mod types;
mod worker_log;

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

use ask::AskContextKind;
use asr::{JsonlTranscriber, Transcriber, WorkerCommand};
use audio::{
    AudioCapture, AudioDevice, AudioEnhancementConfig, CaptureConfig, CpalAudioCapture,
    NoiseSuppressionLevel,
};
use injection::{
    InjectionOptions, InsertResult, InsertionDetail, SelectedText, SystemTextInjector,
    TargetWindow, TextInjector,
};
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
    AppPhase, AppStateSnapshot, DictionaryCandidate, DictionaryEntry, DictionaryEntryInput,
    DictionaryImportInput, GpuDiagnostics, HistoryItem, LoadModelRequest, ModelStatus,
    NewDictionaryEntry, NewHistoryItem, RecordingResult, Settings,
};

const ASR_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const ASR_MODEL_LOAD_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Default)]
struct MicrophoneTestState {
    generation: u64,
    active: Option<u64>,
}

impl MicrophoneTestState {
    fn start(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.active = Some(self.generation);
        self.generation
    }

    fn stop(&mut self) -> bool {
        self.active.take().is_some()
    }

    fn is_current(&self, generation: u64) -> bool {
        self.active == Some(generation)
    }

    fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

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
    edit: tokio::sync::Mutex<Option<EditSession>>,
    ask: tokio::sync::Mutex<Option<AskSession>>,
    answer_panel: answer_panel::AnswerPanelState,
    target: Mutex<Option<TargetWindow>>,
    app_context: Mutex<Option<types::AppContext>>,
    transcriber: Arc<dyn Transcriber>,
    input_monitor: Arc<InputMonitor>,
    model: Mutex<ModelStatus>,
    gpu_diagnostics: Mutex<Option<GpuDiagnostics>>,
    lifecycle: PipelineLifecycle,
    initialization: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    shutdown_started: AtomicBool,
    translation_active: AtomicBool,
    voice_translation_target: Mutex<Option<String>>,
    shortcut_routes: Mutex<shortcuts::Routes>,
    /// Saved shortcuts not dispatched since startup, named by action and
    /// chord: collision losers and chords the OS refused.
    inactive_shortcuts: Mutex<Vec<String>>,
    settings_update: tokio::sync::Mutex<()>,
    microphone_test: tokio::sync::Mutex<MicrophoneTestState>,
}

impl Services {
    fn new(settings: &Settings) -> Self {
        let (model_id, detail) = model_identity(settings);
        Self {
            audio: tokio::sync::Mutex::new(Box::new(CpalAudioCapture::new())),
            live: tokio::sync::Mutex::new(None),
            edit: tokio::sync::Mutex::new(None),
            ask: tokio::sync::Mutex::new(None),
            answer_panel: answer_panel::AnswerPanelState::default(),
            target: Mutex::new(None),
            app_context: Mutex::new(None),
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
            shortcut_routes: Mutex::new(
                shortcuts::Routes::parse_saved(settings).expect("validated settings"),
            ),
            inactive_shortcuts: Mutex::new(Vec::new()),
            settings_update: tokio::sync::Mutex::new(()),
            microphone_test: tokio::sync::Mutex::new(MicrophoneTestState::default()),
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
            self.microphone_test.lock().await.stop();
            let mut audio = self.audio.lock().await;
            let _ = audio.cancel().await;
            let _ = audio.disarm().await;
        }
        if let Some(task) = self.live.lock().await.take() {
            let _ = task.finish().await;
        }
        self.edit.lock().await.take();
        self.ask.lock().await.take();
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

pub(crate) struct EditSession {
    selection: SelectedText,
    injector: SystemTextInjector,
    monitor: Arc<InputMonitor>,
    checkpoint: u64,
}

impl EditSession {
    fn new(settings: &Settings, active_hotkey: &str, from_shortcut: bool) -> Result<Self, String> {
        let injector = SystemTextInjector::new(InjectionOptions {
            restore_clipboard: settings.clipboard_restore,
        });
        let selection = injector
            .capture_selection()
            .map_err(|_| "Select text in a supported foreground edit control.".to_string())?;
        let monitor = Arc::new(InputMonitor::default());
        if !monitor.start_for_recording(active_hotkey, &settings.shortcuts.edit, from_shortcut) {
            return Err("Speak to edit could not monitor the original selection safely.".into());
        }
        let checkpoint = monitor
            .checkpoint()
            .ok_or("Speak to edit could not monitor the original selection safely.")?;
        Ok(Self {
            selection,
            injector,
            monitor,
            checkpoint,
        })
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        self.monitor.shutdown();
    }
}

pub(crate) enum AskCapture {
    Selected(SelectedText),
    Caret(TargetWindow),
    Unavailable,
}

// The answer panel and all other app windows share our process. Never use
// their focused text as a fresh Ask source or insertion target.
fn is_external_ask_target(target: &TargetWindow) -> bool {
    target.process_id != std::process::id()
}

pub(crate) struct AskSession {
    pub(crate) capture: AskCapture,
    pub(crate) injector: SystemTextInjector,
    pub(crate) monitor: Option<Arc<InputMonitor>>,
    pub(crate) checkpoint: Option<u64>,
}

impl AskSession {
    fn new(settings: &Settings, active_hotkey: &str, from_shortcut: bool) -> Self {
        let injector = SystemTextInjector::new(InjectionOptions {
            restore_clipboard: settings.clipboard_restore,
        });
        // Only the known "no selection" result may become a caret capture.
        // Any inaccessible/changed selection becomes panel-only, never an
        // insertion target.
        let capture = match injector.capture_selection() {
            Ok(selection) if is_external_ask_target(selection.target()) => {
                AskCapture::Selected(selection)
            }
            Ok(_) => AskCapture::Unavailable,
            Err(injection::InjectionError::BackendFailure("no text is selected")) => {
                match injector.capture_target() {
                    Ok(target) if is_external_ask_target(&target) => AskCapture::Caret(target),
                    Ok(_) => AskCapture::Unavailable,
                    Err(_) => AskCapture::Unavailable,
                }
            }
            Err(_) => AskCapture::Unavailable,
        };
        let monitor = match &capture {
            AskCapture::Selected(_) | AskCapture::Caret(_) => {
                let monitor = Arc::new(InputMonitor::default());
                if monitor.start_for_recording(
                    active_hotkey,
                    &settings.shortcuts.ask,
                    from_shortcut,
                ) {
                    Some(monitor)
                } else {
                    None
                }
            }
            AskCapture::Unavailable => None,
        };
        let checkpoint = monitor.as_ref().and_then(|monitor| monitor.checkpoint());
        let capture = if matches!(&capture, AskCapture::Caret(_)) && checkpoint.is_none() {
            AskCapture::Unavailable
        } else {
            capture
        };
        Self {
            capture,
            injector,
            monitor,
            checkpoint,
        }
    }

    pub(crate) fn kind(&self) -> AskContextKind {
        match self.capture {
            AskCapture::Selected(_) => AskContextKind::Selected,
            AskCapture::Caret(_) => AskContextKind::Caret,
            AskCapture::Unavailable => AskContextKind::Unavailable,
        }
    }

    pub(crate) fn selected_source(&self) -> Option<&str> {
        match &self.capture {
            AskCapture::Selected(selection) => Some(selection.text()),
            _ => None,
        }
    }
}

impl Drop for AskSession {
    fn drop(&mut self) {
        if let Some(monitor) = &self.monitor {
            monitor.shutdown();
        }
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
    let executable = std::env::current_exe().ok();
    if let Some(directory) = worker_directory(project_root.as_deref(), executable.as_deref()) {
        command = command.with_current_dir(directory);
    }
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

/// The checkout that holds the Python worker. See [`project_root_candidates`]
/// for the search order.
fn project_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok();
    let current_dir = std::env::current_dir().ok();
    find_project_root(
        executable.as_deref(),
        current_dir.as_deref(),
        cfg!(debug_assertions),
    )
}

fn find_project_root(
    executable: Option<&Path>,
    current_dir: Option<&Path>,
    include_current_dir: bool,
) -> Option<PathBuf> {
    project_root_candidates(executable, current_dir, include_current_dir)
        .into_iter()
        .find(|candidate| candidate.join("asr_worker").join("__main__.py").is_file())
}

/// Directories that may be the worker checkout, in priority order: the
/// ancestors of the executable's directory, then (only when
/// `include_current_dir`, i.e. debug builds run by `tauri dev`) the ancestors
/// of the working directory. Release builds ignore the working directory when
/// choosing the checkout, its `.venv`, and its `.env`, so autostart
/// (`C:\Windows\System32`) or a shell in another checkout does not change
/// which checkout is used. Filesystem and drive roots are never candidates.
fn project_root_candidates(
    executable: Option<&Path>,
    current_dir: Option<&Path>,
    include_current_dir: bool,
) -> Vec<PathBuf> {
    let starts = [
        executable.and_then(Path::parent),
        current_dir.filter(|_| include_current_dir),
    ];
    let mut candidates: Vec<PathBuf> = Vec::new();
    for candidate in starts.into_iter().flatten().flat_map(Path::ancestors) {
        if !is_filesystem_root(candidate) && !candidates.iter().any(|known| known == candidate) {
            candidates.push(candidate.to_path_buf());
        }
    }
    candidates
}

/// A drive or filesystem root (`C:\`, `\\server\share\`, `/`), or the empty
/// path that ends the ancestors of a relative path.
fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

/// The worker's working directory. `python -m asr_worker` puts it first on
/// `sys.path`, ahead of `PYTHONPATH`, so the inherited working directory
/// (possibly another checkout) would decide which `asr_worker` is imported.
/// Without a project root, the executable's directory is used instead.
fn worker_directory(project_root: Option<&Path>, executable: Option<&Path>) -> Option<PathBuf> {
    project_root
        .or_else(|| executable.and_then(Path::parent))
        .map(Path::to_path_buf)
}

fn worker_runtime() -> (OsString, Option<PathBuf>) {
    let project_root = project_root();

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

#[cfg(test)]
mod ask_capture_tests {
    use super::*;

    #[test]
    fn ask_never_captures_an_answer_panel_in_its_own_process() {
        let mut target = TargetWindow {
            window_handle: 1,
            control_handle: 1,
            process_id: std::process::id(),
            thread_id: 1,
            is_secure: false,
        };
        assert!(!is_external_ask_target(&target));
        target.process_id = target.process_id.saturating_add(1);
        assert!(is_external_ask_target(&target));
    }
}

fn parse_shortcut(value: &str) -> Result<Shortcut, String> {
    Shortcut::from_str(value.trim()).map_err(|_| "hotkey is invalid".to_string())
}
/// Load the first `.env` found and return content-free startup diagnostics
/// (the chosen worker project root and `.env` path, never any value).
///
/// The text is echoed to stderr for debug runs and, because a release build
/// has no console, written to the worker log once the app installs it.
fn load_environment_file() -> String {
    let current_dir = std::env::current_dir().ok();
    let executable = std::env::current_exe().ok();
    let project_root = project_root();
    let candidates = environment_file_candidates(
        executable.as_deref(),
        project_root.as_deref(),
        current_dir.as_deref(),
        cfg!(debug_assertions),
    );
    let path = candidates.into_iter().find(|path| path.is_file());
    let load_error = path.as_deref().and_then(|path| {
        dotenvy::from_path(path)
            .err()
            .map(|error| describe_environment_error(&error))
    });
    let diagnostics = startup_diagnostics(
        project_root.as_deref(),
        path.as_deref(),
        load_error.as_deref(),
    );
    eprint!("{diagnostics}");
    diagnostics
}

/// Describe a `.env` load failure without the offending line or value, which
/// may hold an API key.
fn describe_environment_error(error: &dotenvy::Error) -> String {
    match error {
        dotenvy::Error::LineParse(_, index) => {
            format!("a line could not be parsed (at character {index})")
        }
        dotenvy::Error::Io(error) => format!("I/O error: {error}"),
        _ => "an environment variable could not be read".into(),
    }
}

fn startup_diagnostics(
    project_root: Option<&Path>,
    environment_file: Option<&Path>,
    load_error: Option<&str>,
) -> String {
    let describe = |path: Option<&Path>| {
        path.map_or_else(
            || "none found".to_string(),
            |path| path.display().to_string(),
        )
    };
    let mut text = format!(
        "Worker project root: {}\nEnvironment file: {}\n",
        describe(project_root),
        describe(environment_file)
    );
    if let Some(error) = load_error {
        text.push_str(&format!("Failed to load the environment file: {error}\n"));
    }
    text
}

#[cfg(test)]
mod startup_diagnostics_tests {
    use super::*;

    #[test]
    fn lists_the_chosen_paths_one_per_line() {
        let root = Path::new("checkout");
        let file = root.join(".env");
        assert_eq!(
            startup_diagnostics(Some(root), Some(&file), None),
            format!(
                "Worker project root: checkout\nEnvironment file: {}\n",
                file.display()
            )
        );
        assert_eq!(
            startup_diagnostics(None, None, None),
            "Worker project root: none found\nEnvironment file: none found\n"
        );
    }

    #[test]
    fn appends_a_load_failure_after_the_paths() {
        let text = startup_diagnostics(None, Some(Path::new(".env")), Some("I/O error: denied"));
        assert!(text.ends_with("Failed to load the environment file: I/O error: denied\n"));
    }

    #[test]
    fn a_parse_failure_never_echoes_the_offending_line() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        std::fs::write(&path, "ASR_API_KEY='sk-secret-value\n").unwrap();
        let error = dotenvy::from_path_iter(&path)
            .unwrap()
            .find_map(Result::err)
            .expect("an unterminated quote is a parse error");
        assert!(error.to_string().contains("sk-secret-value"));
        let description = describe_environment_error(&error);
        assert!(!description.contains("sk-secret"), "{description}");
        assert!(!description.contains("ASR_API_KEY"), "{description}");
    }
}

/// `.env` locations in priority order, following the worker search order:
/// next to the executable, then the checkout that holds the worker (Windows
/// autostart runs the release executable from `C:\Windows\System32`), then,
/// only when `include_current_dir` (debug builds), the working directory and
/// the workspace above `src-tauri`. A `.env` at a filesystem root is never read.
fn environment_file_candidates(
    executable: Option<&Path>,
    project_root: Option<&Path>,
    current_dir: Option<&Path>,
    include_current_dir: bool,
) -> Vec<PathBuf> {
    let current_dir = current_dir.filter(|_| include_current_dir);
    let workspace = current_dir
        .filter(|dir| dir.file_name().is_some_and(|name| name == "src-tauri"))
        .and_then(Path::parent);
    let directories = [
        executable.and_then(Path::parent),
        project_root,
        current_dir,
        workspace,
    ];
    let mut candidates = Vec::new();
    for directory in directories.into_iter().flatten() {
        let path = directory.join(".env");
        if !is_filesystem_root(directory) && !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

#[cfg(test)]
mod microphone_test_tests {
    use super::MicrophoneTestState;

    #[test]
    fn stop_releases_ownership_and_invalidates_old_meter_task() {
        let mut state = MicrophoneTestState::default();
        let first = state.start();
        assert!(state.is_current(first));
        assert!(state.stop());
        assert!(!state.stop());
        assert!(!state.is_current(first));
        let second = state.start();
        assert_ne!(first, second);
        assert!(!state.is_current(first));
        assert!(state.is_current(second));
    }
}

#[cfg(test)]
mod model_configuration_tests {
    use super::*;

    fn filesystem_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })
    }

    #[test]
    fn development_launch_finds_workspace_environment_file() {
        let workspace = Path::new("workspace");
        let current_dir = workspace.join("src-tauri");
        let executable = current_dir.join("target/debug/local-voice-input.exe");

        let candidates =
            environment_file_candidates(Some(&executable), None, Some(&current_dir), true);

        assert!(candidates.contains(&workspace.join(".env")));
    }

    #[test]
    fn autostart_launch_finds_the_checkout_environment_file_last() {
        // Windows autostart starts the release build from System32.
        let workspace = Path::new("workspace");
        let current_dir = Path::new("windows").join("system32");
        let executable = workspace.join("src-tauri/target/release/local-voice-input.exe");

        let candidates = environment_file_candidates(
            Some(&executable),
            Some(workspace),
            Some(&current_dir),
            false,
        );

        assert_eq!(
            candidates,
            [
                workspace.join("src-tauri/target/release/.env"),
                workspace.join(".env"),
            ]
        );
    }

    #[test]
    fn debug_launch_reads_the_working_directory_environment_file_last() {
        let workspace = Path::new("workspace");
        let current_dir = workspace.join("src-tauri");
        let executable = current_dir.join("target/debug/local-voice-input.exe");

        let candidates = environment_file_candidates(
            Some(&executable),
            Some(workspace),
            Some(&current_dir),
            true,
        );

        assert_eq!(
            candidates,
            [
                current_dir.join("target/debug/.env"),
                workspace.join(".env"),
                current_dir.join(".env"),
            ]
        );
    }

    #[test]
    fn environment_file_at_a_filesystem_root_is_never_a_candidate() {
        let root = filesystem_root();
        let executable = root.join("local-voice-input.exe");

        let candidates =
            environment_file_candidates(Some(&executable), Some(&root), Some(&root), true);

        assert!(candidates.is_empty(), "{candidates:?}");
    }

    #[test]
    fn release_project_root_candidates_ignore_the_working_directory() {
        let root = filesystem_root();
        let checkout = root.join("Users").join("me").join("asr");
        let executable = checkout.join("src-tauri/target/release/local-voice-input.exe");
        let system32 = root.join("Windows").join("System32");

        let candidates = project_root_candidates(Some(&executable), Some(&system32), false);

        assert_eq!(
            candidates,
            [
                checkout.join("src-tauri/target/release"),
                checkout.join("src-tauri/target"),
                checkout.join("src-tauri"),
                checkout.clone(),
                root.join("Users").join("me"),
                root.join("Users"),
            ]
        );
    }

    #[test]
    fn debug_project_root_candidates_search_the_working_directory_after_the_executable() {
        let root = filesystem_root();
        let checkout = root.join("asr");
        let executable = checkout.join("src-tauri/target/debug/local-voice-input.exe");
        let system32 = root.join("Windows").join("System32");

        let candidates = project_root_candidates(Some(&executable), Some(&system32), true);

        assert_eq!(
            candidates,
            [
                checkout.join("src-tauri/target/debug"),
                checkout.join("src-tauri/target"),
                checkout.join("src-tauri"),
                checkout.clone(),
                system32.clone(),
                root.join("Windows"),
            ]
        );
    }

    #[test]
    fn project_root_candidates_never_include_a_filesystem_root() {
        let root = filesystem_root();
        let executable = root.join("local-voice-input.exe");

        assert!(project_root_candidates(Some(&executable), Some(&root), true).is_empty());
    }

    fn plant_worker(root: &Path) {
        let package = root.join("asr_worker");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("__main__.py"), "").unwrap();
    }

    #[test]
    fn executable_checkout_wins_over_a_worker_planted_above_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let planted = temp.path().join("planted");
        plant_worker(&checkout);
        plant_worker(&planted);
        let executable = checkout.join("src-tauri/target/release/local-voice-input.exe");
        let current_dir = planted.join("Windows").join("System32");

        for include_current_dir in [false, true] {
            assert_eq!(
                find_project_root(Some(&executable), Some(&current_dir), include_current_dir),
                Some(checkout.clone())
            );
        }
    }

    #[test]
    fn release_build_never_finds_a_worker_only_above_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let planted = temp.path().join("planted");
        plant_worker(&planted);
        let executable = temp
            .path()
            .join("installed/local-voice-input/local-voice-input.exe");
        let current_dir = planted.join("Windows").join("System32");

        assert_eq!(
            find_project_root(Some(&executable), Some(&current_dir), false),
            None
        );
        assert_eq!(
            find_project_root(Some(&executable), Some(&current_dir), true),
            Some(planted)
        );
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
    fn worker_runs_from_the_project_root_or_else_the_executable_directory() {
        let root = filesystem_root();
        let checkout = root.join("asr");
        let executable = checkout.join("src-tauri/target/release/local-voice-input.exe");

        assert_eq!(
            worker_directory(Some(&checkout), Some(&executable)),
            Some(checkout.clone())
        );
        assert_eq!(
            worker_directory(None, Some(&executable)),
            Some(checkout.join("src-tauri/target/release"))
        );
        assert_eq!(worker_directory(None, None), None);
    }

    #[test]
    fn worker_command_does_not_inherit_the_working_directory() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let other = temp.path().join("other");
        plant_worker(&checkout);
        plant_worker(&other);
        let current_dir = other.join("src-tauri");
        let release_dir = checkout.join("src-tauri/target/release");
        let executable = release_dir.join("local-voice-input.exe");

        for include_current_dir in [false, true] {
            let root =
                find_project_root(Some(&executable), Some(&current_dir), include_current_dir);
            let directory = worker_directory(root.as_deref(), Some(&executable));
            assert_eq!(directory.as_ref(), Some(&checkout));
        }

        // Without a checkout above the executable, a release build falls back
        // to the executable's directory rather than the working directory.
        let installed = temp.path().join("installed").join("local-voice-input.exe");
        let root = find_project_root(Some(&installed), Some(&current_dir), false);
        assert_eq!(
            worker_directory(root.as_deref(), Some(&installed)),
            Some(temp.path().join("installed"))
        );

        let command = worker_command_for_settings(&Settings::default());
        assert!(command.current_dir.is_some());
        assert_ne!(command.current_dir, std::env::current_dir().ok());
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
    fn legacy_keep_audio_setting_preserves_stale_captures_but_not_retry_copies() {
        let directory = tempfile::tempdir().unwrap();
        let legacy_capture = directory
            .path()
            .join(format!("local-ai-voice-{}-1-0.wav", u32::MAX));
        let retry_copy = directory.path().join(format!(
            "local-ai-voice-retry-{}-{}.wav",
            u32::MAX,
            "0123456789abcdef0123456789abcdef"
        ));
        fs::write(&legacy_capture, b"legacy").unwrap();
        fs::write(&retry_copy, b"retry").unwrap();

        assert_eq!(
            cleanup_stale_artifacts(&directory.path().join("missing"), false).unwrap(),
            0
        );
        assert_eq!(cleanup_stale_artifacts(directory.path(), false).unwrap(), 1);
        assert!(legacy_capture.exists());
        assert!(!retry_copy.exists());
        assert_eq!(cleanup_stale_artifacts(directory.path(), true).unwrap(), 1);
        assert!(!legacy_capture.exists());
    }

    #[test]
    fn stale_artifact_removal_decision_respects_legacy_keep_audio_setting() {
        assert!(removes_stale_artifact(StaleArtifactKind::Capture, true));
        assert!(!removes_stale_artifact(StaleArtifactKind::Capture, false));
        assert!(removes_stale_artifact(StaleArtifactKind::RetryCopy, true));
        assert!(removes_stale_artifact(StaleArtifactKind::RetryCopy, false));
        assert_eq!(
            stale_artifact(std::ffi::OsStr::new(&format!(
                "local-ai-voice-{}-1-0.wav",
                u32::MAX
            ))),
            Some((u32::MAX, StaleArtifactKind::Capture))
        );
        assert_eq!(
            stale_artifact(std::ffi::OsStr::new(&format!(
                "local-ai-voice-retry-7-{}.wav",
                "0123456789abcdef0123456789abcdef"
            ))),
            Some((7, StaleArtifactKind::RetryCopy))
        );
        assert_eq!(
            stale_artifact(std::ffi::OsStr::new("local-ai-voice-retry-7-short.wav")),
            None
        );
    }
}

/// Temporary audio owned by a process that may have exited without cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StaleArtifactKind {
    /// A capture WAV. Versions before History audio kept these in the
    /// temporary directory when "Delete audio after processing" was off, so
    /// they may be the user's only copy of a recording.
    Capture,
    /// A guarded History Retry copy. It is always cleanup-owned.
    RetryCopy,
}

fn removes_stale_artifact(kind: StaleArtifactKind, delete_audio_after_processing: bool) -> bool {
    match kind {
        StaleArtifactKind::Capture => delete_audio_after_processing,
        StaleArtifactKind::RetryCopy => true,
    }
}

fn stale_artifact(name: &std::ffi::OsStr) -> Option<(u32, StaleArtifactKind)> {
    let name = name.to_str()?.strip_suffix(".wav")?;
    if let Some(retry) = name.strip_prefix("local-ai-voice-retry-") {
        let (process_id, token) = retry.split_once('-')?;
        if token.len() == 32 && token.chars().all(|character| character.is_ascii_hexdigit()) {
            return Some((process_id.parse().ok()?, StaleArtifactKind::RetryCopy));
        }
        return None;
    }
    let components = name
        .strip_prefix("local-ai-voice-")?
        .split('-')
        .collect::<Vec<_>>();
    if components.len() != 3 {
        return None;
    }
    let process_id = components[0].parse().ok()?;
    components[1].parse::<u128>().ok()?;
    components[2].parse::<u64>().ok()?;
    Some((process_id, StaleArtifactKind::Capture))
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
    if !directory.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if stale_artifact(&entry.file_name()).is_some_and(|(process_id, kind)| {
            removes_stale_artifact(kind, delete_audio_after_processing)
                && !process_is_live(process_id)
        }) {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(removed)
}

async fn toggle_voice_mode(app: AppHandle, requested_mode: PipelineMode, trigger_chord: String) {
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
                    Some(trigger_chord),
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
                    Some(trigger_chord),
                )
                .await
            }
            PipelineMode::Edit => {
                commands::start_speak_to_edit_with_origin(
                    app.clone(),
                    app.state::<Services>(),
                    app.state::<AppState>(),
                    app.state::<Storage>(),
                    true,
                    Some(trigger_chord),
                )
                .await
            }
            PipelineMode::Ask => {
                commands::start_ask_with_origin(
                    app.clone(),
                    app.state::<Services>(),
                    app.state::<AppState>(),
                    app.state::<Storage>(),
                    true,
                    Some(trigger_chord),
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
    let route = app
        .state::<Services>()
        .shortcut_routes
        .lock()
        .ok()
        .and_then(|routes| routes.find(shortcut).cloned());
    let Some(route) = route else {
        return;
    };
    match route.action {
        shortcuts::Action::Dictate => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Dictate, route.text))
        }
        shortcuts::Action::Translate => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Translate, route.text))
        }
        shortcuts::Action::Edit => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Edit, route.text))
        }
        shortcuts::Action::Ask => {
            tauri::async_runtime::spawn(toggle_voice_mode(app, PipelineMode::Ask, route.text))
        }
        shortcuts::Action::SelectedText => tauri::async_runtime::spawn(translate_selection(app)),
    };
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

/// Argument the sign-in Run entry passes so the app starts in the tray only.
const AUTOSTART_ARGUMENT: &str = "--autostart";

/// Whether a command line, program path first, came from the sign-in entry.
///
/// The autostart plugin writes the executable path unquoted, so a path with
/// spaces may arrive split across several leading arguments; only an exact
/// argument after the first one counts.
fn launched_by_autostart<I>(arguments: I) -> bool
where
    I: IntoIterator,
    I::Item: AsRef<std::ffi::OsStr>,
{
    arguments
        .into_iter()
        .skip(1)
        .any(|argument| argument.as_ref() == AUTOSTART_ARGUMENT)
}

/// Bring the main window to the front, restoring it from the tray or taskbar.
fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let startup_diagnostics = load_environment_file();
    let builder = tauri::Builder::default();
    // Must stay the first plugin: a second launch hands over to the running
    // instance and exits inside this plugin's initialization, before the
    // database, hotkeys, tray, or ASR worker of the new process start. The
    // lock is keyed on the bundle identifier, so a development build would
    // otherwise exit whenever an installed release build is resident.
    #[cfg(not(debug_assertions))]
    let builder = builder.plugin(tauri_plugin_single_instance::init(
        |app, arguments, _cwd| {
            // A sign-in launch while already running must not pop the window.
            if !launched_by_autostart(&arguments) {
                show_main_window(app);
            }
        },
    ));
    builder
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARGUMENT]),
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
            // Must precede the first worker spawn (initialize_model_runtime).
            // A missing log directory only loses diagnostics, never startup.
            if let Ok(directory) = app.path().app_log_dir() {
                if worker_log::install(&directory).is_ok() {
                    worker_log::record_app_start(&startup_diagnostics);
                }
            }
            let storage = Storage::open(&database_path(app.handle())?)?;
            storage.enforce_current_history_policy()?;
            let settings = storage.get_settings()?;
            // The autostart plugin registers the currently running executable.
            // A development executable depends on Vite's dev server and cannot
            // run on its own at Windows sign-in, so it must never replace the
            // registration created by an installed/release build.
            // enable() rewrites the Run entry on every start, which also adds
            // --autostart to entries registered before the argument existed.
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
            let (routes, shadowed) = shortcuts::Routes::parse_saved_with_shadowed(&settings)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            app.manage(storage);
            app.manage(AppState::default());
            app.manage(Services::new(&settings));
            recording_overlay::create(app.handle())?;
            answer_panel::create(app.handle())?;
            // Collision losers stay inactive until reassigned (older action
            // keeps the chord); the warning names them with refused chords.
            let mut inactive: Vec<String> = shadowed.iter().map(shortcuts::Route::describe).collect();
            let (active_routes, failures) = shortcuts::register_available(&routes, |chord| app.global_shortcut().register(chord));
            *app.state::<Services>().shortcut_routes.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = active_routes;
            if !failures.is_empty() {
                emit_status(app.handle(), "shortcut_registration_failed", "Some saved shortcuts could not be activated at startup. Change them in Settings and restart to verify.");
            }
            for (route, error) in failures {
                eprintln!("Shortcut registration failed for {}: {error}", route.text);
                inactive.push(route.describe());
            }
            *app.state::<Services>().inactive_shortcuts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = inactive;
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
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            // The main window is configured hidden so a sign-in launch stays in
            // the tray; every other launch shows it.
            if !launched_by_autostart(std::env::args_os()) {
                show_main_window(app.handle());
            }
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
            commands::start_microphone_test,
            commands::stop_microphone_test,
            commands::start_voice_translation,
            commands::start_speak_to_edit,
            commands::start_ask,
            commands::stop_recording,
            commands::cancel_recording,
            commands::cycle_voice_translation_target,
            commands::get_app_state,
            commands::get_startup_hotkey_warning,
            commands::get_settings,
            commands::get_shortcut_warning,
            commands::get_model_status,
            commands::load_model,
            commands::get_gpu_diagnostics,
            commands::run_gpu_diagnostics,
            commands::list_history,
            commands::delete_history_item,
            commands::delete_all_history,
            commands::get_history_audio,
            commands::retry_history_item,
            commands::list_dictionary,
            commands::add_dictionary_entry,
            commands::update_dictionary_entry,
            commands::delete_dictionary_entry,
            commands::import_dictionary_csv,
            commands::list_dictionary_candidates,
            commands::confirm_dictionary_candidate,
            commands::reject_dictionary_candidate,
            commands::copy_history_item,
            commands::copy_to_clipboard,
            commands::get_ask_answer,
            commands::dismiss_ask_answer,
            commands::update_settings
        ])
        .on_window_event(|window, event| {
            if window.label() == answer_panel::WINDOW_LABEL {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            if window.label() == "main" {
                // The app stays resident in the tray: closing the main window
                // only hides it, so dictation and the loaded model survive.
                // Tray > Quit exits through RunEvent::ExitRequested, whose
                // asynchronous shutdown stops capture and the ASR worker.
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    // Hiding keeps the Settings page mounted, so it would not
                    // stop a running microphone test and the device would
                    // stay open with no visible UI. The page is told the test
                    // ended so it does not still show it running when the
                    // window is shown again.
                    let app = window.app_handle().clone();
                    tauri::async_runtime::spawn(async move {
                        let services = app.state::<Services>();
                        match commands::release_running_microphone_test(
                            &services.microphone_test,
                            &services.audio,
                        )
                        .await
                        {
                            Ok(true) => {
                                let _ = app.emit(commands::MICROPHONE_TEST_STOPPED_EVENT, ());
                            }
                            Ok(false) => {}
                            Err(error) => emit_status(
                                &app,
                                "microphone_test_failed",
                                &format!("Microphone test failed. {error}"),
                            ),
                        }
                    });
                }
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

#[cfg(test)]
mod autostart_launch_tests {
    use super::*;

    #[test]
    fn a_plain_launch_is_not_an_autostart_launch() {
        assert!(!launched_by_autostart(["local-voice-input.exe"]));
        assert!(!launched_by_autostart(Vec::<String>::new()));
    }

    #[test]
    fn the_sign_in_argument_marks_an_autostart_launch() {
        assert!(launched_by_autostart([
            "local-voice-input.exe",
            "--autostart"
        ]));
    }

    #[test]
    fn an_unquoted_program_path_with_spaces_still_counts() {
        assert!(launched_by_autostart([
            r"C:\Users\me\AppData\Local\Local",
            "Voice",
            r"Input\local-voice-input.exe",
            "--autostart",
        ]));
    }

    #[test]
    fn only_the_exact_argument_after_the_program_counts() {
        assert!(!launched_by_autostart(["--autostart"]));
        assert!(!launched_by_autostart([
            "local-voice-input.exe",
            "--autostart=1"
        ]));
        assert!(!launched_by_autostart([
            "local-voice-input.exe",
            "--AUTOSTART"
        ]));
        assert!(!launched_by_autostart([
            "local-voice-input.exe",
            "--input-monitor"
        ]));
    }

    #[test]
    fn os_string_arguments_are_accepted() {
        let arguments = vec![
            OsString::from("local-voice-input.exe"),
            OsString::from("--autostart"),
        ];
        assert!(launched_by_autostart(arguments));
    }
}

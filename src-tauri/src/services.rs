//! Application-owned services and orderly shutdown.
use crate::{
    answer_panel,
    asr::{JsonlTranscriber, Transcriber},
    audio::{AudioCapture, CpalAudioCapture},
    commands,
    injection::TargetWindow,
    live_dictation, model_identity,
    sessions::{AskSession, EditSession},
    shortcuts,
    state::PipelineLifecycle,
    types::{self, GpuDiagnostics, ModelStatus, Settings},
    worker_command_for_settings, ASR_MODEL_LOAD_TIMEOUT, ASR_REQUEST_TIMEOUT,
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[derive(Default)]
pub(crate) struct MicrophoneTestState {
    generation: u64,
    active: Option<u64>,
}

impl MicrophoneTestState {
    pub(crate) fn start(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.active = Some(self.generation);
        self.generation
    }

    pub(crate) fn stop(&mut self) -> bool {
        self.active.take().is_some()
    }

    pub(crate) fn is_current(&self, generation: u64) -> bool {
        self.active == Some(generation)
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

pub(crate) struct Services {
    pub(crate) audio: tokio::sync::Mutex<Box<dyn AudioCapture>>,
    pub(crate) live: tokio::sync::Mutex<Option<live_dictation::LiveTask>>,
    pub(crate) edit: tokio::sync::Mutex<Option<EditSession>>,
    pub(crate) ask: tokio::sync::Mutex<Option<AskSession>>,
    pub(crate) answer_panel: answer_panel::AnswerPanelState,
    pub(crate) target: Mutex<Option<TargetWindow>>,
    pub(crate) app_context: Mutex<Option<types::AppContext>>,
    pub(crate) transcriber: Arc<dyn Transcriber>,
    pub(crate) model: commands::ModelStatusSlot,
    pub(crate) gpu_diagnostics: Mutex<Option<GpuDiagnostics>>,
    pub(crate) lifecycle: PipelineLifecycle,
    pub(crate) initialization: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
    pub(crate) shutdown_started: AtomicBool,
    pub(crate) voice_translation_target: Mutex<Option<String>>,
    pub(crate) shortcut_routes: Mutex<shortcuts::Routes>,
    /// Saved shortcuts not dispatched since startup, named by action and
    /// chord: collision losers and chords the OS refused.
    pub(crate) inactive_shortcuts: Mutex<Vec<String>>,
    pub(crate) settings_update: tokio::sync::Mutex<()>,
    pub(crate) microphone_test: tokio::sync::Mutex<MicrophoneTestState>,
    /// Whether this app holds the Esc cancel chord. Locked before
    /// `shortcut_routes` whenever both are needed.
    pub(crate) cancel_chord_registered: Mutex<bool>,
}

impl Services {
    pub(crate) fn new(settings: &Settings) -> Self {
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
            model: commands::ModelStatusSlot::new(ModelStatus {
                model_id,
                quantization: settings.model_quantization.clone(),
                installed: false,
                state: "not_loaded".into(),
                detail,
            }),
            gpu_diagnostics: Mutex::new(None),
            lifecycle: PipelineLifecycle::default(),
            initialization: Mutex::new(None),
            shutdown_started: AtomicBool::new(false),
            voice_translation_target: Mutex::new(None),
            shortcut_routes: Mutex::new(
                shortcuts::Routes::parse_saved(settings).expect("validated settings"),
            ),
            inactive_shortcuts: Mutex::new(Vec::new()),
            settings_update: tokio::sync::Mutex::new(()),
            microphone_test: tokio::sync::Mutex::new(MicrophoneTestState::default()),
            cancel_chord_registered: Mutex::new(false),
        }
    }

    pub(crate) fn begin_shutdown(&self) -> bool {
        self.shutdown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) async fn shutdown(&self) {
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
        let _ = self.transcriber.shutdown().await;
    }
}

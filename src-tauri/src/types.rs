use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppPhase {
    Idle,
    Recording,
    Processing,
    Injecting,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppStateSnapshot {
    pub phase: AppPhase,
    pub message: Option<String>,
    pub last_result: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub setup_complete: bool,
    pub hotkey: String,
    pub microphone_id: Option<String>,
    pub history_enabled: bool,
    pub history_retention_days: u32,
    pub delete_audio_after_processing: bool,
    pub auto_start: bool,
    pub clipboard_restore: bool,
    pub model_id: Option<String>,
    #[serde(default = "default_asr_backend")]
    pub asr_backend: String,
}

pub const ASR_BACKENDS: [&str; 3] = ["vibevoice", "faster-whisper", "mock"];

fn default_asr_backend() -> String {
    "vibevoice".into()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            setup_complete: false,
            hotkey: "Ctrl+Shift+Space".into(),
            microphone_id: None,
            history_enabled: true,
            history_retention_days: 30,
            delete_audio_after_processing: true,
            auto_start: false,
            clipboard_restore: true,
            model_id: None,
            asr_backend: default_asr_backend(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LoadModelRequest {
    pub model_id: Option<String>,
    pub quantization: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryItem {
    pub id: i64,
    pub transcript_text: String,
    pub processed_text: Option<String>,
    pub mode: String,
    pub asr_provider: String,
    pub llm_provider: Option<String>,
    pub app_category: Option<String>,
    pub duration_ms: Option<i64>,
    pub latency_ms: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewHistoryItem<'a> {
    pub transcript_text: &'a str,
    pub processed_text: Option<&'a str>,
    pub mode: &'a str,
    pub asr_provider: &'a str,
    pub llm_provider: Option<&'a str>,
    pub app_category: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub latency_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntry {
    pub id: i64,
    pub reading: String,
    pub surface: String,
    pub category: Option<String>,
    pub aliases: Vec<String>,
    pub priority: i64,
    pub app_scope: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct NewDictionaryEntry<'a> {
    pub reading: &'a str,
    pub surface: &'a str,
    pub category: Option<&'a str>,
    pub aliases: &'a [String],
    pub priority: i64,
    pub app_scope: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DictionaryEntryInput {
    pub reading: String,
    pub surface: String,
    pub category: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub priority: i64,
    pub app_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub model_id: Option<String>,
    pub installed: bool,
    pub state: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuDiagnostics {
    pub status: String,
    pub adapter_name: Option<String>,
    pub driver_version: Option<String>,
    pub memory_total_mb: Option<u64>,
    pub recommendation: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordingResult {
    pub text: String,
    pub insertion: String,
    pub duration_ms: u64,
    pub latency_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_use_vibevoice_backend() {
        assert_eq!(Settings::default().asr_backend, "vibevoice");
    }

    #[test]
    fn legacy_settings_without_backend_deserialize_to_default() {
        let stored = r#"{
            "setupComplete": true,
            "hotkey": "Ctrl+Shift+Space",
            "microphoneId": null,
            "historyEnabled": true,
            "historyRetentionDays": 30,
            "deleteAudioAfterProcessing": true,
            "autoStart": false,
            "clipboardRestore": true,
            "modelId": null
        }"#;
        let settings: Settings = serde_json::from_str(stored).unwrap();
        assert_eq!(settings.asr_backend, "vibevoice");
    }
}

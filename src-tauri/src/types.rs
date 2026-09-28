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
    #[serde(default = "default_ui_language")]
    pub ui_language: String,
    pub setup_complete: bool,
    pub hotkey: String,
    #[serde(default = "default_translation_hotkey")]
    pub translation_hotkey: String,
    #[serde(default)]
    pub translation_instruction: String,
    #[serde(default = "default_voice_translate_hotkey")]
    pub voice_translate_hotkey: String,
    #[serde(default = "default_speak_to_edit_hotkey")]
    pub speak_to_edit_hotkey: String,
    #[serde(default = "default_ask_hotkey")]
    pub ask_hotkey: String,
    #[serde(default = "default_translation_target_languages")]
    pub translation_target_languages: Vec<String>,
    #[serde(default = "default_translation_target_language")]
    pub translation_target_language: String,
    pub microphone_id: Option<String>,
    #[serde(default)]
    pub history_retention: HistoryRetention,
    pub delete_audio_after_processing: bool,
    pub auto_start: bool,
    pub clipboard_restore: bool,
    #[serde(default = "default_noise_suppression")]
    pub noise_suppression: String,
    #[serde(default = "default_input_gain_percent")]
    pub input_gain_percent: u16,
    #[serde(default = "default_automatic_gain")]
    pub automatic_gain: bool,
    pub model_id: Option<String>,
    #[serde(default = "default_model_quantization")]
    pub model_quantization: String,
    #[serde(default = "default_asr_backend")]
    pub asr_backend: String,
    #[serde(default = "default_api_base_url")]
    pub api_base_url: String,
    #[serde(default = "default_api_key_env_var")]
    pub api_key_env_var: String,
    #[serde(default)]
    pub text_correction_enabled: bool,
    #[serde(default = "default_correction_provider")]
    pub correction_provider: String,
    #[serde(default = "default_openai_correction_model")]
    pub openai_correction_model: String,
    #[serde(default = "default_openai_reasoning_effort")]
    pub openai_reasoning_effort: String,
    #[serde(default = "default_openai_api_key_env_var")]
    pub openai_api_key_env_var: String,
    #[serde(default = "default_gemini_correction_model")]
    pub gemini_correction_model: String,
    #[serde(default = "default_gemini_api_key_env_var")]
    pub gemini_api_key_env_var: String,
    #[serde(default = "default_local_correction_base_url")]
    pub local_correction_base_url: String,
    #[serde(default = "default_local_correction_model")]
    pub local_correction_model: String,
    #[serde(default = "default_local_correction_max_tokens")]
    pub local_correction_max_tokens: usize,
    #[serde(default = "default_correction_instruction")]
    pub correction_instruction: String,
    #[serde(default = "default_enabled_correction_feature")]
    pub correction_remove_fillers: bool,
    #[serde(default = "default_enabled_correction_feature")]
    pub correction_remove_repetitions: bool,
    #[serde(default = "default_enabled_correction_feature")]
    pub correction_resolve_self_corrections: bool,
    #[serde(default = "default_enabled_correction_feature")]
    pub correction_auto_format: bool,
    #[serde(default = "default_enabled_correction_feature")]
    pub correction_improve_clarity: bool,
    #[serde(default)]
    pub custom_models: Vec<CustomModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CustomModel {
    pub asr_backend: String,
    pub model_id: String,
}

pub const ASR_BACKENDS: [&str; 4] = ["vibevoice", "faster-whisper", "openai-compatible", "mock"];
pub const CORRECTION_PROVIDERS: [&str; 3] = ["openai", "gemini", "local"];
pub const OPENAI_REASONING_EFFORTS: [&str; 6] = ["none", "low", "medium", "high", "xhigh", "max"];
pub const TRANSLATION_TARGET_LANGUAGES: [&str; 8] =
    ["en", "ja", "zh", "es", "fr", "pt", "de", "ko"];

fn default_asr_backend() -> String {
    "faster-whisper".into()
}

fn default_ui_language() -> String {
    "ja".into()
}

fn default_model_quantization() -> String {
    "4bit".into()
}

fn default_api_base_url() -> String {
    "https://api.openai.com/v1".into()
}

fn default_api_key_env_var() -> String {
    "OPENAI_API_KEY".into()
}

fn default_noise_suppression() -> String {
    "medium".into()
}

fn default_correction_provider() -> String {
    "openai".into()
}

fn default_openai_correction_model() -> String {
    "gpt-5.6-luna".into()
}

fn default_openai_reasoning_effort() -> String {
    "none".into()
}

fn default_openai_api_key_env_var() -> String {
    "OPENAI_API_KEY".into()
}

fn default_gemini_correction_model() -> String {
    "gemini-flash-lite-latest".into()
}

fn default_gemini_api_key_env_var() -> String {
    "GEMINI_API_KEY".into()
}

fn default_local_correction_base_url() -> String {
    "http://127.0.0.1:11434/v1".into()
}

fn default_local_correction_model() -> String {
    "qwen3:8b".into()
}

fn default_local_correction_max_tokens() -> usize {
    4096
}

fn default_correction_instruction() -> String {
    String::new()
}

fn default_translation_hotkey() -> String {
    "Ctrl+Shift+T".into()
}

fn default_voice_translate_hotkey() -> String {
    "Ctrl+Shift+Y".into()
}

fn default_speak_to_edit_hotkey() -> String {
    "Ctrl+Shift+E".into()
}
fn default_ask_hotkey() -> String {
    "Ctrl+Shift+A".into()
}

fn default_translation_target_languages() -> Vec<String> {
    vec!["en".into(), "ja".into()]
}

fn default_translation_target_language() -> String {
    "en".into()
}

fn default_enabled_correction_feature() -> bool {
    true
}

fn default_input_gain_percent() -> u16 {
    100
}

fn default_automatic_gain() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_language: default_ui_language(),
            setup_complete: false,
            hotkey: "Ctrl+Shift+Space".into(),
            translation_hotkey: default_translation_hotkey(),
            translation_instruction: String::new(),
            voice_translate_hotkey: default_voice_translate_hotkey(),
            speak_to_edit_hotkey: default_speak_to_edit_hotkey(),
            ask_hotkey: default_ask_hotkey(),
            translation_target_languages: default_translation_target_languages(),
            translation_target_language: default_translation_target_language(),
            microphone_id: None,
            history_retention: HistoryRetention::OneMonth,
            delete_audio_after_processing: true,
            auto_start: false,
            clipboard_restore: true,
            noise_suppression: default_noise_suppression(),
            input_gain_percent: default_input_gain_percent(),
            automatic_gain: default_automatic_gain(),
            model_id: None,
            model_quantization: default_model_quantization(),
            asr_backend: default_asr_backend(),
            api_base_url: default_api_base_url(),
            api_key_env_var: default_api_key_env_var(),
            text_correction_enabled: false,
            correction_provider: default_correction_provider(),
            openai_correction_model: default_openai_correction_model(),
            openai_reasoning_effort: default_openai_reasoning_effort(),
            openai_api_key_env_var: default_openai_api_key_env_var(),
            gemini_correction_model: default_gemini_correction_model(),
            gemini_api_key_env_var: default_gemini_api_key_env_var(),
            local_correction_base_url: default_local_correction_base_url(),
            local_correction_model: default_local_correction_model(),
            local_correction_max_tokens: default_local_correction_max_tokens(),
            correction_instruction: default_correction_instruction(),
            correction_remove_fillers: true,
            correction_remove_repetitions: true,
            correction_resolve_self_corrections: true,
            correction_auto_format: true,
            correction_improve_clarity: true,
            custom_models: Vec::new(),
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
    pub source_text: Option<String>,
    pub instruction_text: Option<String>,
    pub action_kind: Option<String>,
    pub search_site: Option<String>,
    pub mode: String,
    pub asr_provider: String,
    pub llm_provider: Option<String>,
    pub target_language: Option<String>,
    pub app_category: Option<String>,
    pub duration_ms: Option<i64>,
    pub latency_ms: Option<i64>,
    pub created_at: String,
    pub has_audio: bool,
    pub retry_of_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFilter {
    All,
    Dictate,
    Translate,
    Edit,
    Ask,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryAudioPayload {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime_type: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRetention {
    Never,
    #[serde(rename = "24_hours")]
    TwentyFourHours,
    OneWeek,
    #[default]
    OneMonth,
    OneYear,
    Forever,
}

impl HistoryRetention {
    pub fn days(self) -> Option<i64> {
        match self {
            Self::Never => Some(0),
            Self::TwentyFourHours => Some(1),
            Self::OneWeek => Some(7),
            Self::OneMonth => Some(30),
            Self::OneYear => Some(365),
            Self::Forever => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewHistoryItem<'a> {
    pub transcript_text: &'a str,
    pub processed_text: Option<&'a str>,
    pub source_text: Option<&'a str>,
    pub instruction_text: Option<&'a str>,
    pub action_kind: Option<&'a str>,
    pub search_site: Option<&'a str>,
    pub mode: &'a str,
    pub asr_provider: &'a str,
    pub llm_provider: Option<&'a str>,
    pub target_language: Option<&'a str>,
    pub app_category: Option<&'a str>,
    pub duration_ms: Option<i64>,
    pub latency_ms: Option<i64>,
    pub retry_of_id: Option<i64>,
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
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
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
    fn dictionary_entry_input_accepts_frontend_payload_without_optional_fields() {
        let input: DictionaryEntryInput = serde_json::from_str(
            r#"{
                "reading": "open ai",
                "surface": "OpenAI",
                "category": null,
                "aliases": ["ChatGPT"],
                "priority": 0
            }"#,
        )
        .unwrap();

        assert_eq!(input.category, None);
        assert_eq!(input.app_scope, None);
        assert_eq!(input.aliases, vec!["ChatGPT"]);
        assert_eq!(input.priority, 0);
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
        assert_eq!(settings.asr_backend, "faster-whisper");
        assert_eq!(settings.ui_language, "ja");
        assert_eq!(settings.model_quantization, "4bit");
        assert_eq!(settings.api_base_url, "https://api.openai.com/v1");
        assert_eq!(settings.api_key_env_var, "OPENAI_API_KEY");
        assert_eq!(settings.voice_translate_hotkey, "Ctrl+Shift+Y");
        assert_eq!(settings.speak_to_edit_hotkey, "Ctrl+Shift+E");
        assert_eq!(settings.translation_target_languages, ["en", "ja"]);
        assert_eq!(settings.translation_target_language, "en");
        assert!(!settings.text_correction_enabled);
        assert_eq!(settings.correction_provider, "openai");
        assert_eq!(settings.openai_correction_model, "gpt-5.6-luna");
        assert_eq!(settings.openai_reasoning_effort, "none");
        assert_eq!(settings.openai_api_key_env_var, "OPENAI_API_KEY");
        assert_eq!(settings.gemini_correction_model, "gemini-flash-lite-latest");
        assert_eq!(settings.gemini_api_key_env_var, "GEMINI_API_KEY");
        assert_eq!(
            settings.local_correction_base_url,
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(settings.local_correction_model, "qwen3:8b");
        assert_eq!(settings.local_correction_max_tokens, 4096);
        assert!(settings.correction_instruction.is_empty());
        assert!(settings.correction_remove_fillers);
        assert!(settings.correction_remove_repetitions);
        assert!(settings.correction_resolve_self_corrections);
        assert!(settings.correction_auto_format);
        assert!(settings.correction_improve_clarity);
        assert!(settings.custom_models.is_empty());
    }
}

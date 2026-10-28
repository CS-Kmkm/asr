use crate::{
    correction, personalization,
    types::{self, Settings},
};

pub(crate) fn validate_translation_targets(settings: &Settings) -> Result<(), String> {
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

/// The checks of a settings save that depend only on the saved values.
pub(crate) fn validate(settings: &Settings) -> Result<(), String> {
    if settings.translation_instruction.chars().count() > 500
        || settings
            .translation_instruction
            .chars()
            .any(|character| character.is_control() && !['\n', '\r', '\t'].contains(&character))
    {
        return Err("translation instruction must be at most 500 characters and contain no unsupported control characters".into());
    }
    if settings.cloud_consents.len() > 32
        || settings.cloud_consents.iter().any(|key| {
            key.len() > 2048
                || !(matches!(key.as_str(), "openai" | "gemini") || key.starts_with("asr:"))
        })
    {
        return Err("Invalid cloud provider consent.".into());
    }
    personalization::validate_settings_profiles(settings).map_err(str::to_owned)?;
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
    validate_translation_targets(settings)?;
    if !matches!(
        settings.decision_model.as_str(),
        "Cloudflare/clef-flash" | "Cloudflare/clef"
    ) {
        return Err("unsupported local decision model".into());
    }
    if !matches!(
        settings.decision_quantization.as_str(),
        "4bit" | "8bit" | "bf16"
    ) {
        return Err("unsupported decision quantization".into());
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_validation_rejects_an_unselectable_translation_target() {
        let mut settings = Settings::default();
        assert!(settings.validate().is_ok());
        settings.translation_target_languages = vec!["ja".into()];
        settings.translation_target_language = "en".into();
        assert_eq!(
            settings.validate().unwrap_err(),
            "the current translation target must be in the configured list"
        );
        settings.translation_target_language = "ja".into();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn instruction_limits_count_unicode_characters_and_reject_hidden_control_input() {
        let mut settings = Settings {
            translation_instruction: "あ".repeat(500),
            ..Settings::default()
        };
        assert!(settings.validate().is_ok());
        settings.translation_instruction.push('あ');
        assert!(settings
            .validate()
            .unwrap_err()
            .starts_with("translation instruction"));
        settings.translation_instruction = "line one\nline two\tallowed".into();
        assert!(settings.validate().is_ok());
        settings.translation_instruction.push('\0');
        assert!(settings
            .validate()
            .unwrap_err()
            .starts_with("translation instruction"));
    }
}

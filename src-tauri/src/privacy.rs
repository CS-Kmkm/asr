use crate::{state::PipelineMode, types::Settings};

pub(crate) const TEXT_CONSENT_REQUIRED: &str =
    "Allow this AI provider in Privacy before sending text.";
pub(crate) const AUDIO_CONSENT_REQUIRED: &str =
    "Allow this cloud speech provider in Privacy before sending audio.";

pub(crate) fn asr_consent_key(settings: &Settings) -> String {
    format!("asr:{}", settings.api_base_url.trim().trim_end_matches('/'))
}

pub(crate) fn check_asr(settings: &Settings) -> Result<(), &'static str> {
    if settings.asr_backend == "openai-compatible"
        && !settings.cloud_consents.contains(&asr_consent_key(settings))
    {
        return Err(AUDIO_CONSENT_REQUIRED);
    }
    Ok(())
}

pub(crate) fn check_text(settings: &Settings) -> Result<(), &'static str> {
    if matches!(settings.correction_provider.as_str(), "openai" | "gemini")
        && !settings
            .cloud_consents
            .contains(&settings.correction_provider)
    {
        return Err(TEXT_CONSENT_REQUIRED);
    }
    Ok(())
}

pub(crate) fn check_recording(settings: &Settings, mode: PipelineMode) -> Result<(), &'static str> {
    check_asr(settings)?;
    // Optional correction may fall back to local raw dictation without consent.
    if mode != PipelineMode::Dictate {
        check_text(settings)?;
    }
    Ok(())
}

pub(crate) fn live_preview_enabled(settings: &Settings) -> bool {
    if settings.asr_backend == "openai-compatible" {
        return settings.cloud_live_preview_enabled;
    }
    settings
        .live_preview_enabled
        .unwrap_or(settings.asr_backend != "openai-compatible")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_is_required_for_each_provider_and_asr_endpoint() {
        let mut settings = Settings::default();
        assert!(check_recording(&settings, PipelineMode::Dictate).is_ok());
        settings.text_correction_enabled = true;
        assert!(check_recording(&settings, PipelineMode::Dictate).is_ok());
        assert_eq!(check_text(&settings), Err(TEXT_CONSENT_REQUIRED));
        for mode in [
            PipelineMode::Translate,
            PipelineMode::Ask,
            PipelineMode::Edit,
            PipelineMode::SelectedText,
        ] {
            assert_eq!(check_recording(&settings, mode), Err(TEXT_CONSENT_REQUIRED));
        }
        settings.cloud_consents.push("openai".into());
        assert!(check_text(&settings).is_ok());
        settings.correction_provider = "gemini".into();
        assert_eq!(check_text(&settings), Err(TEXT_CONSENT_REQUIRED));
        settings.correction_provider = "local".into();
        assert!(check_text(&settings).is_ok());
        settings.asr_backend = "openai-compatible".into();
        assert_eq!(check_asr(&settings), Err(AUDIO_CONSENT_REQUIRED));
        settings.cloud_consents.push(asr_consent_key(&settings));
        assert!(check_asr(&settings).is_ok());
        settings.api_base_url = "https://another.example/v1".into();
        assert_eq!(check_asr(&settings), Err(AUDIO_CONSENT_REQUIRED));
    }

    #[test]
    fn remote_preview_requires_explicit_opt_in() {
        let mut settings = Settings::default();
        assert!(live_preview_enabled(&settings));
        settings.live_preview_enabled = Some(true);
        settings.asr_backend = "openai-compatible".into();
        assert!(!live_preview_enabled(&settings));
        settings.cloud_live_preview_enabled = true;
        assert!(live_preview_enabled(&settings));
        settings.asr_backend = "vibevoice".into();
        settings.live_preview_enabled = Some(false);
        assert!(!live_preview_enabled(&settings));
    }
}

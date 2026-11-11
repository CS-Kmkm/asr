//! Status and explicit removal of app-managed (pinned speech.cpp) model files.
use super::{MODEL_CHANGE_GATE, MODEL_FILES};
use crate::{
    model_catalog::{self, ManagedModelCache},
    services::Services,
    types::ModelStatus,
};
use tauri::State;

const CACHE_UNAVAILABLE: &str = "The model cache location is unavailable.";
const LOAD_IN_PROGRESS: &str =
    "Wait for the speech model to finish loading before removing a model.";

#[tauri::command]
pub(crate) async fn get_asr_model_cache_status() -> Result<Vec<ManagedModelCache>, String> {
    tokio::task::spawn_blocking(|| {
        let home = model_catalog::speech_cpp_home().ok_or(CACHE_UNAVAILABLE)?;
        Ok(model_catalog::cache_status(&home))
    })
    .await
    .map_err(|_| CACHE_UNAVAILABLE.to_owned())?
}

/// The loaded or loading model must not lose its file underneath it.
pub(crate) fn deletion_blocked(status: &ModelStatus, model_id: &str) -> Option<&'static str> {
    if status.state == "loading" {
        return Some(LOAD_IN_PROGRESS);
    }
    let identity = format!("speech.cpp:{model_id}:Q8_0");
    (status.state == "ready" && status.model_id.as_deref() == Some(identity.as_str())).then_some(
        "This model is loaded. Load a different model before removing its downloaded file.",
    )
}

#[tauri::command]
pub(crate) async fn delete_asr_model_cache(
    model_id: String,
    services: State<'_, Services>,
) -> Result<Vec<ManagedModelCache>, String> {
    if !model_catalog::is_speech_cpp_model(&model_id) {
        return Err("Only managed Japanese speech models can be removed.".into());
    }
    let _settings_update = services.settings_update.lock().await;
    let _change = MODEL_CHANGE_GATE.begin_model_cache_change(|| services.lifecycle.phase())?;
    // An explicit load does not pass the model-change gate; it holds this
    // lock instead. Checked status below is therefore not stale.
    let _files = MODEL_FILES
        .try_write()
        .map_err(|_| LOAD_IN_PROGRESS.to_owned())?;
    if let Some(reason) = deletion_blocked(&services.model.get()?, &model_id) {
        return Err(reason.into());
    }
    tokio::task::spawn_blocking(move || {
        let home = model_catalog::speech_cpp_home().ok_or(CACHE_UNAVAILABLE)?;
        model_catalog::delete_cached_model(&home, &model_id)?;
        Ok(model_catalog::cache_status(&home))
    })
    .await
    .map_err(|_| CACHE_UNAVAILABLE.to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: &str, model_id: Option<&str>) -> ModelStatus {
        ModelStatus {
            model_id: model_id.map(str::to_owned),
            quantization: "4bit".into(),
            installed: state == "ready",
            state: state.into(),
            detail: String::new(),
        }
    }

    #[test]
    fn a_running_load_excludes_cache_removal() {
        let _load = MODEL_FILES
            .try_read()
            .expect("no removal holds the lock in tests");
        assert!(MODEL_FILES.try_write().is_err());
    }

    #[test]
    fn loaded_or_loading_models_cannot_be_removed() {
        let haas = "haas-ja-v1.0";
        assert!(deletion_blocked(&status("loading", None), haas).is_some());
        assert!(
            deletion_blocked(&status("ready", Some("speech.cpp:haas-ja-v1.0:Q8_0")), haas)
                .is_some()
        );
        assert!(deletion_blocked(
            &status("ready", Some("speech.cpp:parakeet-tdt_ctc-0.6b-ja:Q8_0")),
            haas
        )
        .is_none());
        assert!(deletion_blocked(
            &status("ready", Some("faster-whisper:large-v3-turbo")),
            haas
        )
        .is_none());
        assert!(deletion_blocked(
            &status("not_loaded", Some("speech.cpp:haas-ja-v1.0:Q8_0")),
            haas
        )
        .is_none());
    }
}

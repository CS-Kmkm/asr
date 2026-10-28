//! Main-window commands expose key presence, never existing secret values.
use super::MODEL_CHANGE_GATE;
use crate::{
    command_error,
    credentials::{self as keys, CredentialStatus, Provider},
    services::Services,
    storage::Storage,
    worker_command_with_speech_key,
};
use tauri::{AppHandle, Emitter, Manager, State};

#[tauri::command]
pub(crate) async fn get_provider_credentials(
    storage: State<'_, Storage>,
) -> Result<Vec<CredentialStatus>, String> {
    let storage = storage.inner().clone();
    tokio::task::spawn_blocking(move || {
        let settings = storage.get_settings().map_err(command_error)?;
        Ok([Provider::Openai, Provider::Gemini, Provider::Speech]
            .into_iter()
            .map(|provider| keys::status(provider, &settings))
            .collect())
    })
    .await
    .map_err(|_| {
        "Windows Credential Manager is unavailable. Check Windows credentials and try again."
            .to_owned()
    })?
}

#[tauri::command]
pub(crate) async fn update_provider_credential(
    app: AppHandle,
    provider: Provider,
    expected_alias: String,
    secret: Option<String>,
) -> Result<CredentialStatus, String> {
    // The owner survives an abandoned IPC response: a native key write cannot
    // be aborted, and must finish reconfiguration before the gate is released.
    tauri::async_runtime::spawn(update_owned(app, provider, expected_alias, secret))
        .await
        .map_err(|_| {
            "Windows Credential Manager is unavailable. Check Windows credentials and try again."
                .to_owned()
        })?
}

async fn update_owned(
    app: AppHandle,
    provider: Provider,
    expected_alias: String,
    secret: Option<String>,
) -> Result<CredentialStatus, String> {
    let services = app.state::<Services>();
    let _settings_update = services.settings_update.lock().await;
    let _change = MODEL_CHANGE_GATE.begin_secret_change(|| services.lifecycle.phase())?;
    let storage = app.state::<Storage>().inner().clone();
    let (status, command) = tokio::task::spawn_blocking(move || {
        let settings = storage.get_settings().map_err(command_error)?;
        let alias = provider
            .checked_alias(&settings, &expected_alias)
            .map_err(|error| error.to_string())?;
        let active_speech_alias = settings.asr_backend == "openai-compatible"
            && alias.eq_ignore_ascii_case(settings.api_key_env_var.trim());
        let live_key = secret.clone().or_else(|| crate::environment::value(&alias));
        // Prepare the new command before committing; use the acknowledged
        // new secret directly instead of a second store lookup/fallback race.
        let command =
            active_speech_alias.then(|| worker_command_with_speech_key(&settings, live_key));
        let deleting = secret.is_none();
        match secret {
            Some(secret) => keys::save(&alias, secret),
            None => keys::delete(&alias),
        }
        .map_err(|error| error.to_string())?;
        let status = CredentialStatus {
            provider,
            alias: alias.clone(),
            store_available: true,
            source: if !deleting {
                keys::Source::CredentialManager
            } else if crate::environment::value(&alias).is_some_and(|s| !s.trim().is_empty()) {
                keys::Source::Environment
            } else {
                keys::Source::Missing
            },
        };
        Ok::<_, String>((status, command))
    })
    .await
    .map_err(|_| {
        "Windows Credential Manager is unavailable. Check Windows credentials and try again."
            .to_owned()
    })??;
    // Aliases may be shared by speech and text providers. Refresh the remote
    // worker whenever the changed key is the speech alias, even from a text row.
    if let Some(command) = command {
        services.transcriber.reconfigure(command).await;
        // Residency observers update model status after reconfiguration;
        // the next normal operation loads the worker with the new credential.
    }
    let _ = app.emit("provider-credentials-changed", ());
    Ok(status)
}

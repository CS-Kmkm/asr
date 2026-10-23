//! Provider HTTP execution. Endpoint/key overrides exist only in test builds.
use super::{
    api_error, api_key, await_reqwest_or_cancel, collect_response, gemini_request,
    local_chat_completions_url, local_client, local_request, openai_request, CorrectionError,
    OutputBudget, GEMINI_INTERACTIONS_URL, OPENAI_RESPONSES_URL, REQUEST_TIMEOUT,
};
use crate::types::Settings;
use reqwest::{redirect::Policy, Client};
use std::sync::OnceLock;
use tokio::sync::watch;

pub(super) struct TextRequest<'a> {
    pub input: &'a str,
    pub instruction: &'a str,
    pub trim_output: bool,
    pub budget: OutputBudget,
}

pub(super) enum ProviderAccess {
    Configured,
    #[cfg(test)]
    Stub {
        endpoint: String,
        api_key: String,
    },
}

impl ProviderAccess {
    fn key(&self, environment: &str) -> Result<String, CorrectionError> {
        match self {
            Self::Configured => api_key(environment),
            #[cfg(test)]
            Self::Stub { api_key, .. } => Ok(api_key.clone()),
        }
    }
    fn endpoint<'a>(&'a self, configured: &'a str) -> &'a str {
        match self {
            Self::Configured => configured,
            #[cfg(test)]
            Self::Stub { endpoint, .. } => endpoint,
        }
    }
    fn client(&self, local: bool) -> Result<Client, reqwest::Error> {
        #[cfg(test)]
        if matches!(self, Self::Stub { .. }) {
            return Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(Policy::none())
                .no_proxy()
                .build();
        }
        // Reuse connection pools. Auth headers remain per request so a changed
        // provider key never needs to rebuild or invalidate a client.
        static LOCAL: OnceLock<Client> = OnceLock::new();
        static CLOUD: OnceLock<Client> = OnceLock::new();
        let slot = if local { &LOCAL } else { &CLOUD };
        if let Some(client) = slot.get() {
            return Ok(client.clone());
        }
        let client = if local {
            local_client()?
        } else {
            Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(Policy::none())
                .build()?
        };
        let _ = slot.set(client.clone());
        Ok(client)
    }
}

pub(super) async fn request_text_with_access(
    settings: &Settings,
    task: TextRequest<'_>,
    mut cancel: watch::Receiver<bool>,
    mut on_update: impl FnMut(&str),
    access: ProviderAccess,
) -> Result<String, CorrectionError> {
    if *cancel.borrow() {
        return Err(CorrectionError::Cancelled);
    }
    crate::privacy::check_text(settings).map_err(|_| CorrectionError::ConsentRequired)?;
    let TextRequest {
        input,
        instruction,
        trim_output,
        budget,
    } = task;
    let provider = settings.correction_provider.as_str();
    let client = access.client(provider == "local")?;
    let (request, sent_key) = match provider {
        "openai" => {
            let key = access.key(&settings.openai_api_key_env_var)?;
            let body = openai_request(settings, input, instruction, budget);
            let request = client
                .post(access.endpoint(OPENAI_RESPONSES_URL))
                .bearer_auth(&key)
                .json(&body);
            (request, Some(key))
        }
        "gemini" => {
            let key = access.key(&settings.gemini_api_key_env_var)?;
            let body = gemini_request(settings, input, instruction, budget);
            let request = client
                .post(access.endpoint(GEMINI_INTERACTIONS_URL))
                .header("x-goog-api-key", &key)
                .json(&body);
            (request, Some(key))
        }
        "local" => (
            client
                .post(local_chat_completions_url(
                    &settings.local_correction_base_url,
                )?)
                .json(&local_request(settings, input, instruction, budget)),
            None,
        ),
        provider => return Err(CorrectionError::UnsupportedProvider(provider.into())),
    };
    let response = await_reqwest_or_cancel(request.send(), &mut cancel).await?;
    let status = response.status();
    if !status.is_success() {
        let mut body = await_reqwest_or_cancel(response.text(), &mut cancel).await?;
        if let Some(key) = sent_key.filter(|key| !key.is_empty()) {
            body = body.replace(&key, "[redacted]");
        }
        return Err(api_error(status, &body));
    }
    let result = collect_response(response, provider, &mut cancel, &mut on_update).await?;
    let result = if trim_output { result.trim() } else { &result };
    if result.is_empty() {
        return Err(CorrectionError::InvalidResponse(
            "the model returned empty text".into(),
        ));
    }
    Ok(result.to_owned())
}

#[cfg(test)]
mod tests;

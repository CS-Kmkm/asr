//! Provider HTTP execution. Endpoint/key overrides exist only in test builds.
use super::{
    api_error, api_key, await_reqwest_or_cancel, collect_response, gemini_request,
    local_chat_completions_url, local_client, local_request, openai_request, CorrectionError,
    OutputBudget, GEMINI_INTERACTIONS_URL, OPENAI_RESPONSES_URL, REQUEST_TIMEOUT,
};
use crate::types::Settings;
use reqwest::{redirect::Policy, Client};
use std::{sync::OnceLock, time::Duration};
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
        client: Option<Client>,
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
            if let Self::Stub {
                client: Some(client),
                ..
            } = self
            {
                return Ok(client.clone());
            }
            return cloud_client_with(REQUEST_TIMEOUT, true);
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
            cloud_client_with(REQUEST_TIMEOUT, false)?
        };
        let _ = slot.set(client.clone());
        Ok(client)
    }
}

fn cloud_client_with(idle_timeout: Duration, no_proxy: bool) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .connect_timeout(super::LOCAL_CONNECT_TIMEOUT)
        .read_timeout(idle_timeout)
        .redirect(Policy::none());
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder.build()
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
    let template = request.build()?;
    let cloud = provider != "local";
    // The read timeout measures silence. A separate generous total budget
    // scales with output size and covers both attempts together.
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs((budget.visible_tokens(input) as u64 / 2 + 60).clamp(180, 1_800));
    for attempt in 0..=1 {
        let mut request = template.try_clone().ok_or_else(|| {
            CorrectionError::InvalidResponse("provider request cannot be cloned".into())
        })?;
        if cloud {
            *request.timeout_mut() =
                Some(deadline.saturating_duration_since(tokio::time::Instant::now()));
        }
        let response = match await_reqwest_or_cancel(client.execute(request), &mut cancel).await {
            Ok(response) => response,
            Err(CorrectionError::Request(_)) if cloud && attempt == 0 => {
                retry_wait(None, &mut cancel).await?;
                continue;
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        if !status.is_success() {
            if cloud && attempt == 0 && (status.as_u16() == 429 || status.is_server_error()) {
                if let Some(delay) = short_retry_after(
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()),
                ) {
                    drop(response);
                    retry_wait(Some(delay), &mut cancel).await?;
                    continue;
                }
            }
            let mut body = await_reqwest_or_cancel(response.text(), &mut cancel).await?;
            if let Some(key) = sent_key.as_ref().filter(|key| !key.is_empty()) {
                body = body.replace(key, "[redacted]");
            }
            return Err(api_error(status, &body));
        }
        let mut emitted = false;
        let result = collect_response(response, provider, &mut cancel, &mut |delta| {
            emitted |= !delta.is_empty();
            on_update(delta);
        })
        .await;
        let result = match result {
            Err(CorrectionError::Request(_)) if cloud && attempt == 0 && !emitted => {
                retry_wait(None, &mut cancel).await?;
                continue;
            }
            result => result?,
        };
        let result = if trim_output { result.trim() } else { &result };
        if result.is_empty() {
            return Err(CorrectionError::InvalidResponse(
                "the model returned empty text".into(),
            ));
        }
        return Ok(result.to_owned());
    }
    unreachable!("each final attempt returns its result")
}

fn short_retry_after(value: Option<&str>) -> Option<Duration> {
    let Some(value) = value else {
        return Some(Duration::from_millis(250));
    };
    let seconds = value.parse::<u64>().ok().or_else(|| {
        let retry_at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
        Some((retry_at.timestamp() - chrono::Utc::now().timestamp()).max(0) as u64)
    })?;
    // Preserve longer server backoffs by returning the original failure
    // rather than retrying earlier than requested or blocking for minutes.
    (seconds <= 2).then(|| Duration::from_secs(seconds))
}

async fn retry_wait(
    retry_after: Option<Duration>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<(), CorrectionError> {
    let mut jitter = [0u8; 1];
    let _ = getrandom::fill(&mut jitter);
    let delay = retry_after.unwrap_or(Duration::from_millis(250))
        + Duration::from_millis(u64::from(jitter[0] % 100));
    if *cancel.borrow() {
        return Err(CorrectionError::Cancelled);
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = cancel.changed() => Err(CorrectionError::Cancelled),
    }
}

#[cfg(test)]
mod tests;

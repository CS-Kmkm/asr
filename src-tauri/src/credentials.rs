//! Provider credentials stay in the OS store, outside Settings and SQLite.
//! One thread owns all native calls, preserving Windows entry sequencing.
use crate::types::Settings;
use serde::{Deserialize, Serialize};
use std::sync::{mpsc, OnceLock};

#[derive(Debug, Clone, Copy, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CredentialError {
    #[error("Windows Credential Manager is unavailable. Check Windows credentials and try again.")]
    Unavailable,
    #[error(
        "Enter a non-empty API key without whitespace or control characters (up to 2048 bytes)."
    )]
    InvalidKey,
    #[error("The API key environment variable name is invalid.")]
    InvalidName,
    #[error(
        "API key settings changed. Review the current environment variable name and try again."
    )]
    SettingsChanged,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Provider {
    Openai,
    Gemini,
    Speech,
}

impl Provider {
    pub(crate) fn checked_alias(
        self,
        settings: &Settings,
        expected: &str,
    ) -> Result<String, CredentialError> {
        let alias = normalized_name(self.alias(settings))?;
        if alias != normalized_name(expected)? {
            return Err(CredentialError::SettingsChanged);
        }
        Ok(alias)
    }
    pub(crate) fn alias(self, settings: &Settings) -> &str {
        match self {
            Self::Openai => &settings.openai_api_key_env_var,
            Self::Gemini => &settings.gemini_api_key_env_var,
            Self::Speech => &settings.api_key_env_var,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Source {
    CredentialManager,
    Environment,
    Missing,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredentialStatus {
    pub provider: Provider,
    pub alias: String,
    pub source: Source,
    pub store_available: bool,
}

fn normalized_name(name: &str) -> Result<String, CredentialError> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|c| c == b'_' || c.is_ascii_alphanumeric())
    {
        return Err(CredentialError::InvalidName);
    }
    Ok(name.to_ascii_uppercase())
}

fn validate_key(key: &str) -> Result<(), CredentialError> {
    if key.is_empty() || key.len() > 2048 || !key.bytes().all(|c| c.is_ascii_graphic()) {
        return Err(CredentialError::InvalidKey);
    }
    Ok(())
}

enum Operation {
    Get(String),
    Set(String, String),
    Delete(String),
}
type Reply = Result<Option<String>, CredentialError>;
trait Backend: Send + 'static {
    fn apply(&mut self, operation: Operation) -> Reply;
}

struct Actor {
    requests: mpsc::Sender<(Operation, mpsc::Sender<Reply>)>,
}

impl Actor {
    fn start(mut backend: impl Backend) -> Result<Self, CredentialError> {
        let (requests, incoming) = mpsc::channel::<(Operation, mpsc::Sender<Reply>)>();
        std::thread::Builder::new()
            .name("provider-credentials".into())
            .spawn(move || {
                while let Ok((operation, reply)) = incoming.recv() {
                    // No Debug or display of operations: Set carries the secret.
                    let _ = reply.send(backend.apply(operation));
                }
            })
            .map_err(|_| CredentialError::Unavailable)?;
        Ok(Self { requests })
    }

    fn request(&self, operation: Operation) -> Reply {
        let (reply, response) = mpsc::channel();
        self.requests
            .send((operation, reply))
            .map_err(|_| CredentialError::Unavailable)?;
        // Native calls cannot be cancelled safely. Wait for the known
        // outcome; a timeout could commit a key after its owner let starts in.
        response.recv().map_err(|_| CredentialError::Unavailable)?
    }
}

#[cfg(windows)]
struct Native;

#[cfg(windows)]
impl Backend for Native {
    fn apply(&mut self, operation: Operation) -> Reply {
        let name = match &operation {
            Operation::Get(name) | Operation::Set(name, _) | Operation::Delete(name) => name,
        };
        // Initialize keyring's Windows store, then explicitly choose local
        // persistence rather than the native adapter's Enterprise default.
        keyring::Entry::store_status()
            .as_ref()
            .map_err(|_| CredentialError::Unavailable)?;
        let service = if cfg!(debug_assertions) {
            "LocalVoiceInput-development"
        } else {
            "LocalVoiceInput-production"
        };
        let entry = keyring_core::Entry::new_with_modifiers(
            service,
            name,
            &std::collections::HashMap::from([("persistence", "Local")]),
        )
        .map_err(|_| CredentialError::Unavailable)?;
        match operation {
            Operation::Get(_) => match entry.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(CredentialError::Unavailable),
            },
            Operation::Set(_, secret) => entry
                .set_password(&secret)
                .map(|()| None)
                .map_err(|_| CredentialError::Unavailable),
            Operation::Delete(_) => match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(None),
                Err(_) => Err(CredentialError::Unavailable),
            },
        }
    }
}

#[cfg(not(windows))]
struct Native;
#[cfg(not(windows))]
impl Backend for Native {
    fn apply(&mut self, operation: Operation) -> Reply {
        match operation {
            Operation::Get(_) => Ok(None),
            _ => Err(CredentialError::Unavailable),
        }
    }
}

fn actor() -> Result<&'static Actor, CredentialError> {
    static ACTOR: OnceLock<Result<Actor, CredentialError>> = OnceLock::new();
    ACTOR
        .get_or_init(|| Actor::start(Native))
        .as_ref()
        .map_err(|error| *error)
}

fn stored(name: &str) -> Reply {
    let name = normalized_name(name)?;
    // Automated tests never inspect a developer's real stored credentials.
    #[cfg(test)]
    {
        let _ = name;
        Ok(None)
    }
    #[cfg(not(test))]
    {
        actor()?.request(Operation::Get(name))
    }
}

fn resolve(stored: Reply, fallback: Option<String>) -> Option<String> {
    stored
        .ok()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fallback.filter(|value| !value.trim().is_empty()))
}

pub(crate) fn key(name: &str) -> Option<String> {
    resolve(stored(name), crate::environment::value(name.trim()))
}

pub(crate) fn status(provider: Provider, settings: &Settings) -> CredentialStatus {
    let name = provider.alias(settings);
    let stored = stored(name);
    let store_available = cfg!(windows) && stored.is_ok();
    let source = if stored.ok().flatten().is_some_and(|s| !s.trim().is_empty()) {
        Source::CredentialManager
    } else if crate::environment::value(name.trim()).is_some_and(|s| !s.trim().is_empty()) {
        Source::Environment
    } else {
        Source::Missing
    };
    CredentialStatus {
        provider,
        alias: name.trim().to_ascii_uppercase(),
        source,
        store_available,
    }
}

pub(crate) fn save(name: &str, secret: String) -> Result<(), CredentialError> {
    let name = normalized_name(name)?;
    validate_key(&secret)?;
    actor()?.request(Operation::Set(name, secret)).map(|_| ())
}

pub(crate) fn delete(name: &str) -> Result<(), CredentialError> {
    actor()?
        .request(Operation::Delete(normalized_name(name)?))
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;
    struct Fake {
        entries: HashMap<String, String>,
        thread: Option<std::thread::ThreadId>,
    }
    impl Backend for Fake {
        fn apply(&mut self, op: Operation) -> Reply {
            let thread = std::thread::current().id();
            assert_eq!(*self.thread.get_or_insert(thread), thread);
            match op {
                Operation::Get(name) => Ok(self.entries.get(&name).cloned()),
                Operation::Set(name, secret) => {
                    self.entries.insert(name, secret);
                    Ok(None)
                }
                Operation::Delete(name) => {
                    self.entries.remove(&name);
                    Ok(None)
                }
            }
        }
    }
    #[test]
    fn native_operations_are_ordered_on_one_thread_and_changes_are_immediate() {
        let actor = Actor::start(Fake {
            entries: HashMap::new(),
            thread: None,
        })
        .unwrap();
        assert_eq!(actor.request(Operation::Get("KEY".into())).unwrap(), None);
        actor
            .request(Operation::Set("KEY".into(), "first".into()))
            .unwrap();
        assert_eq!(
            actor
                .request(Operation::Get("KEY".into()))
                .unwrap()
                .as_deref(),
            Some("first")
        );
        actor
            .request(Operation::Set("KEY".into(), "second".into()))
            .unwrap();
        assert_eq!(
            actor
                .request(Operation::Get("KEY".into()))
                .unwrap()
                .as_deref(),
            Some("second")
        );
        actor.request(Operation::Delete("KEY".into())).unwrap();
        assert_eq!(actor.request(Operation::Get("KEY".into())).unwrap(), None);
    }

    #[test]
    fn a_native_write_cannot_report_failure_while_it_can_still_commit() {
        struct Blocked {
            ready: mpsc::Sender<()>,
            resume: mpsc::Receiver<()>,
        }
        impl Backend for Blocked {
            fn apply(&mut self, operation: Operation) -> Reply {
                assert!(matches!(operation, Operation::Set(_, _)));
                self.ready.send(()).unwrap();
                self.resume.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(None)
            }
        }
        let (ready, entered) = mpsc::channel();
        let (resume, blocked) = mpsc::channel();
        let actor = Actor::start(Blocked {
            ready,
            resume: blocked,
        })
        .unwrap();
        let (result, outcome) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            result
                .send(actor.request(Operation::Set("KEY".into(), "value".into())))
                .unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            outcome.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        resume.send(()).unwrap();
        assert_eq!(
            outcome.recv_timeout(Duration::from_secs(2)).unwrap(),
            Ok(None)
        );
        writer.join().unwrap();
    }
    #[test]
    fn stored_keys_take_precedence_and_environment_remains_a_fallback() {
        assert_eq!(
            resolve(Ok(Some("stored".into())), Some("environment".into())).as_deref(),
            Some("stored")
        );
        for absent in [Ok(None), Err(CredentialError::Unavailable)] {
            assert_eq!(
                resolve(absent, Some("environment".into())).as_deref(),
                Some("environment")
            );
        }
        assert_eq!(normalized_name(" Custom_KEY ").unwrap(), "CUSTOM_KEY");
        assert_eq!(normalized_name("1_CUSTOM_KEY").unwrap(), "1_CUSTOM_KEY");
        assert!(normalized_name("bad.name").is_err());
        for key in ["", "key with spaces", "key\nheader", &"x".repeat(2049)] {
            assert!(validate_key(key).is_err());
        }
        assert!(validate_key("test-api-key_123").is_ok());
    }

    #[test]
    fn stale_form_cannot_write_a_different_saved_alias() {
        let mut settings = Settings::default();
        assert_eq!(
            Provider::Openai
                .checked_alias(&settings, "openai_api_key")
                .unwrap(),
            "OPENAI_API_KEY"
        );
        settings.openai_api_key_env_var = "NEW_PROVIDER_KEY".into();
        assert_eq!(
            Provider::Openai.checked_alias(&settings, "OPENAI_API_KEY"),
            Err(CredentialError::SettingsChanged)
        );
        assert_eq!(Provider::Speech.alias(&settings), settings.api_key_env_var);
    }
}

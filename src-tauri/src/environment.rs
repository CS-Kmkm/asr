//! Application configuration is not automatically inherited by child processes.
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    path::Path,
    sync::{OnceLock, RwLock},
};

static FILE_VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();
static PROVIDER_KEYS: OnceLock<RwLock<HashSet<String>>> = OnceLock::new();

pub(crate) fn register_provider_keys(settings: &crate::types::Settings) {
    let mut names = PROVIDER_KEYS
        .get_or_init(|| RwLock::new(HashSet::new()))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for name in [
        &settings.api_key_env_var,
        &settings.openai_api_key_env_var,
        &settings.gemini_api_key_env_var,
    ] {
        names.insert(name.trim().to_ascii_uppercase());
    }
}

pub(crate) fn is_provider_key(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    matches!(
        name.as_str(),
        "OPENAI_API_KEY" | "GEMINI_API_KEY" | "ASR_API_KEY"
    ) || PROVIDER_KEYS.get().is_some_and(|names| {
        names
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&name)
    })
}

fn read_file_values(path: &Path) -> Result<HashMap<String, String>, dotenvy::Error> {
    let mut values = HashMap::new();
    for entry in dotenvy::from_path_iter(path)? {
        let (key, value) = entry?;
        values.entry(configuration_key(&key)).or_insert(value);
    }
    Ok(values)
}

fn configuration_key(name: &str) -> String {
    if cfg!(windows) {
        name.to_ascii_uppercase()
    } else {
        name.into()
    }
}

fn configured_value(values: &HashMap<String, String>, name: &str) -> Option<String> {
    values.get(&configuration_key(name)).cloned()
}

pub(crate) fn load(path: &Path) -> Result<(), dotenvy::Error> {
    let values = read_file_values(path)?;
    let _ = FILE_VALUES.set(values);
    Ok(())
}

pub(crate) fn value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| configured_value(FILE_VALUES.get()?, name))
}

pub(crate) fn value_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).or_else(|| value(name).map(OsString::from))
}

pub(crate) fn allowed_for_child(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    if ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|part| name.contains(part))
    {
        return false;
    }
    matches!(
        name.as_str(),
        "PATH"
            | "PATHEXT"
            | "SYSTEMROOT"
            | "SYSTEMDRIVE"
            | "PROCESSOR_ARCHITECTURE"
            | "USERNAME"
            | "WINDIR"
            | "COMSPEC"
            | "TEMP"
            | "TMP"
            | "TMPDIR"
            | "HOME"
            | "USERPROFILE"
            | "APPDATA"
            | "LOCALAPPDATA"
            | "PROGRAMDATA"
            | "PROGRAMFILES"
            | "PROGRAMFILES(X86)"
            | "LANG"
            | "LC_ALL"
            | "PYTHONPATH"
            | "PYTHONHOME"
            | "PYTHONUTF8"
            | "PYTHONIOENCODING"
            | "HF_HOME"
            | "HF_ENDPOINT"
            | "XDG_CACHE_HOME"
            | "KMP_DUPLICATE_LIB_OK"
            | "HF_HUB_CACHE"
            | "HF_HUB_OFFLINE"
            | "HF_HUB_DISABLE_TELEMETRY"
            | "HUGGINGFACE_HUB_CACHE"
            | "TRANSFORMERS_CACHE"
            | "TORCH_HOME"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
            | "REQUESTS_CA_BUNDLE"
            | "LD_LIBRARY_PATH"
            | "DYLD_LIBRARY_PATH"
            | "OMP_NUM_THREADS"
            | "MKL_NUM_THREADS"
    ) || name.starts_with("ASR_")
        || name.starts_with("HF_HUB_")
        || name.starts_with("CT2_")
        || name.starts_with("CUDA_")
        || name.starts_with("CUDNN_")
        || name.starts_with("PYTORCH_")
}

pub(crate) fn child_environment() -> Vec<(OsString, OsString)> {
    process_environment(false)
}

pub(crate) fn helper_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    command.env_clear().envs(child_environment());
    command
}

pub(crate) fn worker_environment() -> Vec<(OsString, OsString)> {
    process_environment(true)
}

fn process_environment(worker: bool) -> Vec<(OsString, OsString)> {
    let inherited = std::env::vars_os();
    let configured = FILE_VALUES
        .get()
        .into_iter()
        .flat_map(|values| values.iter())
        .map(|(key, value)| (OsString::from(key), OsString::from(value)));
    // Parent process variables win over .env, matching value().
    let mut values: HashMap<OsString, OsString> = HashMap::new();
    for (key, value) in configured.chain(inherited) {
        let key = if cfg!(windows) {
            OsString::from(key.to_string_lossy().to_ascii_uppercase())
        } else {
            key
        };
        values.insert(key, value);
    }
    select_child_values(values, worker)
}

fn select_child_values(
    values: impl IntoIterator<Item = (OsString, OsString)>,
    worker: bool,
) -> Vec<(OsString, OsString)> {
    values
        .into_iter()
        .filter(|(key, _)| {
            let name = key.to_string_lossy();
            !is_provider_key(&name)
                && (allowed_for_child(&name)
                    || (worker
                        && matches!(
                            name.to_ascii_uppercase().as_str(),
                            "HF_TOKEN" | "HUGGING_FACE_HUB_TOKEN"
                        )))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_commands_receive_only_the_filtered_environment() {
        for program in ["input-monitor", "nvidia-smi"] {
            let command = helper_command(program);
            assert!(command.get_envs().all(|(name, value)| value.is_none() || {
                let name = name.to_string_lossy();
                allowed_for_child(&name) && !is_provider_key(&name)
            }));
        }
    }

    #[test]
    fn custom_provider_names_are_excluded_and_huggingface_auth_is_worker_only() {
        let settings = crate::types::Settings {
            api_key_env_var: "ASR_REGION_35".into(),
            ..crate::types::Settings::default()
        };
        register_provider_keys(&settings);
        let values = [
            "ASR_REGION_35",
            "ASR_KEY",
            "ASR_APIKEY",
            "ASR_OPENAI_KEY",
            "OPENAI_API_KEY",
            "HF_ENDPOINT",
            "HF_TOKEN",
            "KMP_DUPLICATE_LIB_OK",
            "CT2_VERBOSE",
        ]
        .map(|name| (OsString::from(name), OsString::from("synthetic-value")));
        let helpers = select_child_values(values.clone(), false);
        let worker = select_child_values(values, true);
        for name in [
            "ASR_REGION_35",
            "ASR_KEY",
            "ASR_APIKEY",
            "ASR_OPENAI_KEY",
            "OPENAI_API_KEY",
        ] {
            assert!(!helpers.iter().any(|(key, _)| key == name));
            assert!(!worker.iter().any(|(key, _)| key == name));
        }
        assert!(!helpers.iter().any(|(key, _)| key == "HF_TOKEN"));
        for name in [
            "HF_ENDPOINT",
            "HF_TOKEN",
            "KMP_DUPLICATE_LIB_OK",
            "CT2_VERBOSE",
        ] {
            assert!(worker.iter().any(|(key, _)| key == name));
        }
    }

    #[test]
    fn children_receive_runtime_configuration_without_provider_secrets() {
        for name in [
            "PATH",
            "SystemRoot",
            "ASR_PYTHON",
            "ASR_DEVICE",
            "CUDA_PATH",
            "HF_HOME",
        ] {
            assert!(allowed_for_child(name), "{name}");
        }
        for name in [
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "ASR_API_KEY",
            "ASR_OTHER_API_KEY",
            "HF_TOKEN",
            "UNRELATED_CONFIGURATION",
        ] {
            assert!(!allowed_for_child(name), "{name}");
        }
    }

    #[test]
    fn reading_dotenv_does_not_modify_the_parent_environment() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        let name = "ASR_TEST_PRIVATE_DOTENV_VALUE_4F37";
        std::fs::write(&path, format!("{name}=private-value\n")).unwrap();
        let values = read_file_values(&path).unwrap();
        assert_eq!(values.get(name).map(String::as_str), Some("private-value"));
        assert!(std::env::var_os(name).is_none());
    }

    #[test]
    fn dotenv_preserves_first_keys_and_windows_case_insensitive_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(".env");
        std::fs::write(
            &path,
            "ASR_TEST_DUPLICATE_35=first\nASR_TEST_DUPLICATE_35=second\n",
        )
        .unwrap();
        let values = read_file_values(&path).unwrap();
        assert_eq!(
            configured_value(&values, "ASR_TEST_DUPLICATE_35").as_deref(),
            Some("first")
        );
        assert_eq!(
            configured_value(&values, "asr_test_duplicate_35").as_deref(),
            if cfg!(windows) { Some("first") } else { None }
        );
    }
}

//! The shared ASR model catalog (`src/asr-model-catalog.json`).
//!
//! The UI renders capabilities from the same file, the Python worker keeps
//! its own pinned artifact table (cross-checked by a worker test), and this
//! module derives speech.cpp model validation and managed-cache handling from
//! it. Only pinned speech.cpp model files are managed here; the shared runtime
//! and Hugging Face caches are never touched.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Display-only fields are parsed so the catalog tests can check them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Catalog {
    pub schema_version: u32,
    pub models: Vec<CatalogModel>,
}

// Display-only fields are parsed so the catalog tests can check them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogModel {
    pub id: String,
    pub backend: Option<String>,
    pub model_id: Option<String>,
    pub availability: String,
    pub language_scope: Option<String>,
    pub inference: Vec<String>,
    pub devices: Vec<String>,
    pub source: Option<CatalogSource>,
    pub weight_format: Option<String>,
}

// Display-only fields are parsed so the catalog tests can check them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogSource {
    pub repo: String,
    pub revision: Option<String>,
    pub file: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
}

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../../src/asr-model-catalog.json"))
            .expect("the bundled ASR model catalog is valid JSON")
    })
}

/// A pinned, app-managed model file (currently the speech.cpp models).
pub(crate) struct ManagedModel<'a> {
    pub model_id: &'a str,
    pub revision: &'a str,
    pub file: &'a str,
    pub size_bytes: u64,
}

pub(crate) fn managed_models() -> impl Iterator<Item = ManagedModel<'static>> {
    catalog().models.iter().filter_map(|model| {
        if model.availability != "available" || model.backend.as_deref() != Some("speech-cpp") {
            return None;
        }
        let source = model.source.as_ref()?;
        Some(ManagedModel {
            model_id: model.model_id.as_deref()?,
            revision: source.revision.as_deref()?,
            file: source.file.as_deref()?,
            size_bytes: source.size_bytes?,
        })
    })
}

pub(crate) fn is_speech_cpp_model(model_id: &str) -> bool {
    managed_models().any(|model| model.model_id == model_id)
}

/// Mirrors `asr_worker.speech_cpp.cache_home`: an empty override falls back
/// to the default. A relative override would resolve against the worker's
/// working directory, which differs from this process's, so it is refused
/// rather than guessed (status reports the location as unavailable).
pub(crate) fn speech_cpp_home() -> Option<PathBuf> {
    let value = |name| crate::environment::value_os(name).filter(|value| !value.is_empty());
    // An empty LOCALAPPDATA is not skipped: the worker would use it as a
    // relative path, so it resolves to "unavailable" here.
    let local_app_data = crate::environment::value_os("LOCALAPPDATA");
    resolve_speech_cpp_home(value("ASR_SPEECH_CPP_HOME"), local_app_data, {
        // Python's Path.home(): USERPROFILE on Windows, HOME elsewhere.
        if cfg!(windows) {
            value("USERPROFILE")
        } else {
            value("HOME")
        }
    })
}

fn resolve_speech_cpp_home(
    override_home: Option<std::ffi::OsString>,
    local_app_data: Option<std::ffi::OsString>,
    user_home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    if let Some(home) = override_home {
        let home = PathBuf::from(home);
        return home.is_absolute().then_some(home);
    }
    let base = local_app_data
        .map(PathBuf::from)
        .or_else(|| user_home.map(|home| PathBuf::from(home).join(".cache")))?;
    base.is_absolute()
        .then(|| base.join("LocalVoice").join("speech.cpp"))
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManagedModelCache {
    pub model_id: String,
    /// `present` (expected size; SHA256 is verified again at load),
    /// `incomplete` (unexpected size), `missing`, or `unsafe` (not a plain file).
    pub state: String,
    pub size_bytes: Option<u64>,
    pub expected_bytes: u64,
}

fn model_path(home: &Path, model: &ManagedModel<'_>) -> PathBuf {
    home.join(model.revision).join(model.file)
}

fn plain_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

pub(crate) fn cache_status(home: &Path) -> Vec<ManagedModelCache> {
    managed_models()
        .map(|model| {
            let path = model_path(home, &model);
            let (state, size_bytes) = match std::fs::symlink_metadata(&path) {
                Err(_) => ("missing", None),
                Ok(meta) if !meta.is_file() || !plain_directory(&home.join(model.revision)) => {
                    ("unsafe", None)
                }
                Ok(meta) if meta.len() == model.size_bytes => ("present", Some(meta.len())),
                Ok(meta) => ("incomplete", Some(meta.len())),
            };
            ManagedModelCache {
                model_id: model.model_id.into(),
                state: state.into(),
                size_bytes,
                expected_bytes: model.size_bytes,
            }
        })
        .collect()
}

/// Removes one pinned model file. Returns whether a file was removed. Never
/// follows links, never removes directories, staging files or the runtime.
pub(crate) fn delete_cached_model(home: &Path, model_id: &str) -> Result<bool, String> {
    let model = managed_models()
        .find(|model| model.model_id == model_id)
        .ok_or_else(|| "Only managed Japanese speech models can be removed.".to_string())?;
    let folder = home.join(model.revision);
    let path = model_path(home, &model);
    let meta = match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err("The model file could not be inspected.".into()),
        Ok(meta) => meta,
    };
    if !meta.is_file() || !plain_directory(&folder) {
        return Err("The model cache entry is not a regular file; it was left unchanged.".into());
    }
    std::fs::remove_file(&path).map_err(|_| {
        "The model file could not be removed. It may be in use; switch to another model and try again."
            .to_string()
    })?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex64(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }

    #[test]
    fn catalog_entries_are_well_formed() {
        let catalog = catalog();
        assert_eq!(catalog.schema_version, 1);
        let mut ids = std::collections::BTreeSet::new();
        for model in &catalog.models {
            assert!(ids.insert(model.id.as_str()), "duplicate id {}", model.id);
            assert!(
                ["available", "planned"].contains(&model.availability.as_str()),
                "{}",
                model.id
            );
            assert!(model.language_scope.as_deref().is_none_or(|scope| {
                ["japanese_only", "multilingual", "server_defined"].contains(&scope)
            }));
            for mode in &model.inference {
                // Stateful streaming is a separate capability (#82) that no
                // implemented engine provides yet; never advertise it here.
                assert!(
                    ["batch", "chunked"].contains(&mode.as_str()),
                    "{}",
                    model.id
                );
            }
            for device in &model.devices {
                assert!(["cpu", "cuda", "vulkan", "remote"].contains(&device.as_str()));
            }
            if model.availability == "available" {
                let backend = model
                    .backend
                    .as_deref()
                    .expect("available model has a backend");
                assert!(crate::types::ASR_BACKENDS.contains(&backend));
                assert!(model.inference.contains(&"batch".to_string()));
                assert!(!model.devices.is_empty());
                // Incremental final recognition is local-only.
                assert_eq!(
                    model.inference.contains(&"chunked".to_string()),
                    backend != "openai-compatible",
                    "{}",
                    model.id
                );
                // speech.cpp's GPU path is Vulkan; it must never read as CUDA.
                if backend == "speech-cpp" {
                    assert_eq!(model.devices, ["vulkan"]);
                    assert_eq!(model.language_scope.as_deref(), Some("japanese_only"));
                    assert_eq!(model.weight_format.as_deref(), Some("Q8_0"));
                    let source = model.source.as_ref().expect("pinned source");
                    assert!(!source.repo.is_empty());
                    assert!(source.revision.as_deref().is_some_and(|r| r.len() == 40));
                    assert!(source.sha256.as_deref().is_some_and(hex64));
                    assert!(source
                        .file
                        .as_deref()
                        .is_some_and(|f| f.ends_with(".gguf") && !f.contains(['/', '\\'])));
                    assert!(source.size_bytes.is_some_and(|size| size > 0));
                }
            } else {
                assert!(model.backend.is_none() && model.inference.is_empty());
            }
        }
    }

    #[test]
    fn speech_cpp_ids_come_from_the_catalog() {
        let ids: Vec<_> = managed_models().map(|model| model.model_id).collect();
        assert_eq!(ids, ["haas-ja-v1.0", "parakeet-tdt_ctc-0.6b-ja"]);
        assert!(is_speech_cpp_model("haas-ja-v1.0"));
        assert!(!is_speech_cpp_model("large-v3-turbo"));
        assert!(!is_speech_cpp_model("planned:qwen3-asr-0.6b"));
    }

    #[test]
    fn cache_home_matches_the_worker_resolution() {
        let absolute = std::env::temp_dir();
        let os = |path: &Path| Some(path.as_os_str().to_owned());
        assert_eq!(
            resolve_speech_cpp_home(os(&absolute), os(Path::new("ignored")), None),
            Some(absolute.clone())
        );
        assert_eq!(
            resolve_speech_cpp_home(Some("relative/cache".into()), os(&absolute), None),
            None
        );
        assert_eq!(
            resolve_speech_cpp_home(None, os(&absolute), None),
            Some(absolute.join("LocalVoice").join("speech.cpp"))
        );
        assert_eq!(
            resolve_speech_cpp_home(None, None, os(&absolute)),
            Some(
                absolute
                    .join(".cache")
                    .join("LocalVoice")
                    .join("speech.cpp")
            )
        );
        assert_eq!(resolve_speech_cpp_home(None, None, None), None);
        assert_eq!(
            resolve_speech_cpp_home(None, Some("".into()), os(&absolute)),
            None
        );
    }

    fn haas() -> ManagedModel<'static> {
        managed_models().next().unwrap()
    }

    #[test]
    fn cache_status_reports_missing_present_and_incomplete() {
        let home = tempfile::tempdir().unwrap();
        let status = cache_status(home.path());
        assert!(status.iter().all(|entry| entry.state == "missing"));

        let model = haas();
        let path = model_path(home.path(), &model);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"partial").unwrap();
        let entry = cache_status(home.path()).remove(0);
        assert_eq!(entry.state, "incomplete");
        assert_eq!(entry.size_bytes, Some(7));

        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(model.size_bytes).unwrap();
        drop(file);
        let entry = cache_status(home.path()).remove(0);
        assert_eq!(entry.state, "present");
        assert_eq!(entry.expected_bytes, model.size_bytes);
    }

    #[test]
    fn delete_removes_only_the_pinned_file() {
        let home = tempfile::tempdir().unwrap();
        let model = haas();
        let path = model_path(home.path(), &model);
        let folder = path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(&path, b"weights").unwrap();
        let staging = folder.join(".download-1-abcdefgh");
        std::fs::write(&staging, b"partial").unwrap();
        let runtime = home.path().join("runtime-0.10.1");
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::write(runtime.join("speech.exe"), b"exe").unwrap();

        assert_eq!(delete_cached_model(home.path(), model.model_id), Ok(true));
        assert!(!path.exists());
        assert!(staging.exists() && runtime.join("speech.exe").exists() && folder.is_dir());
        assert_eq!(delete_cached_model(home.path(), model.model_id), Ok(false));
    }

    #[test]
    fn delete_rejects_unknown_ids_and_non_files() {
        let home = tempfile::tempdir().unwrap();
        for id in [
            "large-v3-turbo",
            "../haas-ja-v1.0",
            "",
            "planned:qwen3-asr-0.6b",
        ] {
            assert!(delete_cached_model(home.path(), id).is_err(), "{id}");
        }
        let model = haas();
        let path = model_path(home.path(), &model);
        std::fs::create_dir_all(&path).unwrap();
        assert!(delete_cached_model(home.path(), model.model_id).is_err());
        assert!(path.is_dir());
        assert_eq!(cache_status(home.path())[0].state, "unsafe");
    }
}

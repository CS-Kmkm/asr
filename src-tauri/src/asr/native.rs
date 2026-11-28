//! Direct child-process adapter for speech.cpp's public JSONL v4 protocol.
//! The desktop supervisor still owns cancellation, timeouts and process reaping.

use super::{
    AsrError, WorkerCommand, WorkerError, PROTOCOL_VERSION, REQUEST_WRITE_TIMEOUT, WAV_HEADER_BYTES,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::OnceLock,
};
use tokio::{io::AsyncWriteExt, process::ChildStdin};

pub(super) mod artifacts;
#[cfg(all(test, windows))]
mod tests;

const VERSION: &str = "0.10.1";
const MODEL_ID: &str = "haas-ja-v1.0";
const MAX_WAV_BYTES: u64 = 48_000_044;
const CHUNK_BYTES: usize = 8192;
static BUNDLED_RUNTIME: OnceLock<PathBuf> = OnceLock::new();

pub(crate) fn register_bundled_runtime(resources: &Path) {
    let runtime = resources
        .join("speech-cpp")
        .join(format!("runtime-{VERSION}"));
    if cfg!(windows) && runtime.is_dir() {
        let _ = BUNDLED_RUNTIME.set(runtime);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Speech {
    artifacts: [(PathBuf, &'static str); 3],
    device: &'static str,
    model_id: &'static str,
    model_size: u64,
    japanese_only: bool,
    accepts_prompt: bool,
    bundled_runtime: bool,
    cache_home: PathBuf,
    offline_only: bool,
}

pub(crate) fn command(settings: &crate::types::Settings) -> Option<WorkerCommand> {
    if !cfg!(windows) || settings.asr_backend != "speech-cpp" {
        return None;
    }
    let model_id = settings.model_id.as_deref().unwrap_or(MODEL_ID);
    let device = match crate::environment::value_os("ASR_NATIVE_SPEECH_CPP")
        .as_deref()
        .and_then(std::ffi::OsStr::to_str)
    {
        Some("cpu") => "CPU",
        Some("vulkan") => "Vulkan0",
        _ => return None,
    };
    let home = crate::model_catalog::speech_cpp_home()?;
    let model = crate::model_catalog::pinned_model("speech-cpp", model_id)?;
    if model.weight_format.as_deref() != Some("Q8_0")
        || !model.inference.iter().any(|kind| kind == "batch")
    {
        return None;
    }
    let japanese_only = match model.language_scope.as_deref()? {
        "japanese_only" => true,
        "multilingual" => false,
        _ => return None,
    };
    let accepts_prompt = model.vocabulary.as_deref() == Some("prompt");
    let source = model.source.as_ref()?;
    let model_id = model.model_id.as_deref()?;
    let model_size = source.size_bytes?;
    let bundled_runtime = BUNDLED_RUNTIME.get();
    let runtime = bundled_runtime
        .cloned()
        .unwrap_or_else(|| home.join(format!("runtime-{VERSION}")));
    let executable = runtime.join("speech.exe");
    let weights = home
        .join(source.revision.as_deref()?)
        .join(source.file.as_deref()?);
    Some(WorkerCommand {
        program: executable.clone(),
        args: vec![
            "worker".into(),
            weights.to_str()?.into(),
            "--device".into(),
            device.into(),
        ],
        env: Vec::new(),
        current_dir: Some(runtime.clone()),
        quantization: Some(settings.model_quantization.clone()),
        provision: None,
        source_required: false,
        native: Some(Speech {
            artifacts: [
                (
                    executable,
                    "46f264e26fcaa60f433455c6c6092137c123af67fad956d67f250daf5ebaa893",
                ),
                (
                    runtime.join("speech.dll"),
                    "923fb0babff5596202ae92dc5b6423a23579ab461bc7cce8b35258107ee05662",
                ),
                (weights, source.sha256.as_deref()?),
            ],
            device,
            model_id,
            model_size,
            japanese_only,
            accepts_prompt,
            bundled_runtime: bundled_runtime.is_some(),
            cache_home: home,
            offline_only: settings.offline_only,
        }),
    })
}

#[cfg(windows)]
pub(super) async fn spawn_owned(
    command: &mut tokio::process::Command,
) -> Result<(tokio::process::Child, std::os::windows::io::OwnedHandle), AsrError> {
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

    let ownership_error = || {
        worker_error(
            "speech_cpp_unavailable",
            "Unable to own the native speech.cpp worker process",
        )
    };
    // SAFETY: The null descriptor gives us a non-inheritable owned handle.
    let job = unsafe {
        OwnedHandle::from_raw_handle(
            CreateJobObjectW(None, None)
                .map_err(|_| ownership_error())?
                .0,
        )
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: The information pointer and size match the information class.
    unsafe {
        SetInformationJobObject(
            HANDLE(job.as_raw_handle()),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|_| ownership_error())?;
    let mut child = command
        .creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0)
        .spawn()?;
    let result = (|| {
        let process = child.raw_handle().ok_or_else(ownership_error)?;
        // SAFETY: Both handles remain live through assignment.
        unsafe { AssignProcessToJobObject(HANDLE(job.as_raw_handle()), HANDLE(process)) }
            .map_err(|_| ownership_error())?;
        resume_primary_thread(child.id().ok_or_else(ownership_error)?)
            .map_err(|_| ownership_error())
    })();
    if let Err(error) = result {
        let _ = child.kill().await;
        return Err(error);
    }
    Ok((child, job))
}

#[cfg(windows)]
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{
        GetProcessIdOfThread, OpenThread, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
        THREAD_SUSPEND_RESUME,
    };

    // std drops the primary thread handle returned by CreateProcessW. A
    // suspended child has a single thread, which Tool Help can enumerate.
    #[repr(C)]
    struct ThreadEntry {
        size: u32,
        usage: u32,
        thread_id: u32,
        owner_pid: u32,
        base_priority: i32,
        delta_priority: i32,
        flags: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
        fn Thread32First(snapshot: HANDLE, entry: *mut ThreadEntry) -> i32;
        fn Thread32Next(snapshot: HANDLE, entry: *mut ThreadEntry) -> i32;
    }
    const TH32CS_SNAPTHREAD: u32 = 4;
    // SAFETY: Tool Help initializes entries with the specified struct size.
    // Own both handles so every return path closes them.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        let _snapshot_owner = OwnedHandle::from_raw_handle(snapshot.0);
        let mut entry: ThreadEntry = std::mem::zeroed();
        entry.size = size_of::<ThreadEntry>() as u32;
        let mut found = Thread32First(snapshot, &mut entry);
        while found != 0 {
            if entry.owner_pid == pid {
                let thread = OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    false,
                    entry.thread_id,
                )?;
                let _thread_owner = OwnedHandle::from_raw_handle(thread.0);
                if GetProcessIdOfThread(thread) != pid || ResumeThread(thread) != 1 {
                    return Err(io::Error::other("Unable to resume suspended worker"));
                }
                return Ok(());
            }
            entry.size = size_of::<ThreadEntry>() as u32;
            found = Thread32Next(snapshot, &mut entry);
        }
        Err(io::Error::last_os_error())
    }
}

impl Speech {
    pub(super) fn installer(&self) -> Result<WorkerCommand, AsrError> {
        if self.offline_only {
            return Err(worker_error(
                "model_not_cached",
                "Offline mode requires the model and runtime already cached",
            ));
        }
        let settings = crate::types::Settings {
            asr_backend: "speech-cpp".into(),
            model_id: Some(self.model_id.into()),
            offline_only: false,
            ..Default::default()
        };
        let mut installer = crate::python_worker_command(&settings, None);
        if installer.source_required
            && !installer
                .current_dir
                .as_ref()
                .is_some_and(|root| root.join("asr_worker/speech_cpp.py").is_file())
        {
            return Err(worker_error(
                "worker_source_missing",
                "Speech worker resources are missing",
            ));
        }
        let mode = if self.bundled_runtime {
            "--prepare-model-only"
        } else {
            "--prepare-only"
        };
        installer.args = vec!["-m".into(), "asr_worker.speech_cpp".into(), mode.into()];
        Ok(installer)
    }

    pub(super) fn cache_home(&self) -> &std::path::Path {
        &self.cache_home
    }
}

pub(super) struct Protocol {
    speech: Speech,
    local: Option<Vec<u8>>,
    active: Option<(u64, u64)>,
    loaded: bool,
}

impl Protocol {
    pub(super) fn ready(speech: Speech, line: &[u8]) -> Result<Self, AsrError> {
        let ready: Value = serde_json::from_slice(line).map_err(protocol_error)?;
        if ready["type"] != "ready"
            || ready["protocol"] != 4
            || ready["version"] != VERSION
            || ready["model"]["task"] != "recognition"
            || ready["model"]["device"] != speech.device
            || ready["model"]["weight_type"] != "Q8_0"
        {
            return Err(protocol_error("invalid speech.cpp readiness response"));
        }
        Ok(Self {
            speech,
            local: None,
            active: None,
            loaded: false,
        })
    }

    pub(super) fn take_local(&mut self) -> Option<Vec<u8>> {
        self.local.take()
    }

    pub(super) async fn send(
        &mut self,
        request: &Value,
        stdin: &mut Option<ChildStdin>,
        early_reply: impl Future<Output = Result<Vec<u8>, AsrError>>,
    ) -> Result<(), AsrError> {
        let id = request["id"]
            .as_u64()
            .ok_or_else(|| protocol_error("invalid request id"))?;
        let mut reply = json!({"id": id, "ok": true});
        match request["command"].as_str() {
            Some("hello") => {
                reply["protocol_version"] = json!(PROTOCOL_VERSION);
                reply["worker_version"] = json!(env!("CARGO_PKG_VERSION"));
            }
            Some("health") => {
                reply["health"] = health(self.loaded.then_some(self.speech.device));
            }
            Some("load") if request["quantization"] == "8bit" => self.loaded = true,
            Some("load") => {
                reply = failure(
                    id,
                    "speech_cpp_unavailable",
                    "The native speech.cpp model requires 8-bit weights",
                )
            }
            Some("transcribe") if self.loaded => {
                let language =
                    request["language"]
                        .as_str()
                        .unwrap_or(if self.speech.japanese_only {
                            "ja"
                        } else {
                            "auto"
                        });
                if self.speech.japanese_only && !matches!(language, "auto" | "ja" | "ja-JP") {
                    self.local = Some(
                        serde_json::to_vec(&failure(
                            id,
                            "speech_cpp_language",
                            "This native speech.cpp model recognizes Japanese only",
                        ))
                        .map_err(protocol_error)?,
                    );
                    return Ok(());
                }
                let path = request["audio_path"]
                    .as_str()
                    .ok_or_else(|| protocol_error("missing WAV path"))?;
                let path = PathBuf::from(path);
                let (wav, rate) = tokio::task::spawn_blocking(move || {
                    if fs::metadata(&path)?.len() > MAX_WAV_BYTES {
                        return Err(protocol_error("WAV exceeds the native worker input limit"));
                    }
                    crate::audio::read_pcm16_wav_file(&path).map_err(protocol_error)
                })
                .await
                .map_err(|_| AsrError::Crashed)??;
                let pipe = stdin.as_mut().ok_or(AsrError::Crashed)?;
                let target = id.to_string();
                let pcm = &wav[WAV_HEADER_BYTES as usize..];
                self.active = Some((id, (pcm.len() as u64 * 500) / u64::from(rate)));
                let upload = async {
                    for (seq, chunk) in pcm.chunks(CHUNK_BYTES).enumerate() {
                        send(
                            pipe,
                            json!({"type":"chunk","id":&target,"seq":seq,"pcm":STANDARD.encode(chunk)}),
                        )
                        .await?;
                    }
                    Ok::<(), AsrError>(())
                };
                tokio::select! {
                    biased;
                    response = early_reply => {
                        let line = response?;
                        let reply = self.receive(&line)?;
                        return match reply {
                            Some(reply) => match super::JsonlTranscriber::validate_response(&reply, &json!(id)) {
                                Err(error) => Err(error),
                                Ok(_) => Err(protocol_error("native transcription ended during upload")),
                            },
                            None => Err(protocol_error("unexpected native progress during upload")),
                        };
                    }
                    result = upload => result?,
                }
                let mut options =
                    json!({"type":"transcribe","id":target,"sample_rate":rate,"language":language});
                if self.speech.accepts_prompt {
                    if let Some(prompt) = request["prompt"].as_str().filter(|p| !p.is_empty()) {
                        options["prompt"] = json!(prompt);
                    }
                }
                send(pipe, options).await?;
                return Ok(());
            }
            Some("cancel") => {
                let target = request["target_id"]
                    .as_u64()
                    .ok_or_else(|| protocol_error("invalid cancel target"))?;
                let pipe = stdin.as_mut().ok_or(AsrError::Crashed)?;
                send(pipe, json!({"type":"cancel","id":target.to_string()})).await?;
                // The target's terminal response is still drained by the supervisor.
            }
            Some("shutdown") => {
                stdin.take();
            }
            _ => {
                reply = failure(
                    id,
                    "unsupported_operation",
                    "The operation is unavailable in the native speech worker",
                )
            }
        }
        self.local = Some(serde_json::to_vec(&reply).map_err(protocol_error)?);
        Ok(())
    }

    pub(super) fn receive(&mut self, line: &[u8]) -> Result<Option<Vec<u8>>, AsrError> {
        let response: Value = serde_json::from_slice(line).map_err(protocol_error)?;
        let (id, duration) = self
            .active
            .ok_or_else(|| protocol_error("unsolicited native worker response"))?;
        if response["id"].as_str() != Some(id.to_string().as_str()) {
            return Err(protocol_error("mismatched native worker response"));
        }
        let reply = match response["type"].as_str() {
            Some("progress") => return Ok(None),
            Some("end") if response["stop"] == "complete" => {
                let text = response["text"]
                    .as_str()
                    .ok_or_else(|| protocol_error("missing native transcript"))?;
                json!({"id":id,"ok":true,"text":text,"segments":[],"model":format!("speech.cpp:{}:Q8_0", self.speech.model_id),"duration_ms":duration})
            }
            Some("end") if response["stop"] == "model_limit" => failure(
                id,
                "transcript_truncated",
                "Native speech.cpp reached its model limit",
            ),
            Some("cancelled") => failure(id, "cancelled", "Native transcription was cancelled"),
            Some("error") if response["error"]["code"] == "out_of_memory" => {
                let code = if self.speech.device == "CPU" {
                    "native_cpu_oom"
                } else {
                    "native_gpu_oom"
                };
                return Err(worker_error(code, "Native speech.cpp ran out of memory"));
            }
            Some("error") if response["error"]["code"] == "internal" => {
                return Err(AsrError::Crashed);
            }
            Some("error") => failure(
                id,
                "speech_cpp_unavailable",
                "Native speech.cpp could not transcribe this recording",
            ),
            _ => return Err(protocol_error("unexpected native worker response")),
        };
        self.active = None;
        Ok(Some(serde_json::to_vec(&reply).map_err(protocol_error)?))
    }
}

pub(super) fn health(device: Option<&str>) -> Value {
    json!({
        "backend": "speech-cpp",
        "loaded": device.is_some(),
        "pythonPath": "Not used",
        "pythonVersion": "Native worker",
        "device": device.map(|device| if device == "CPU" { "cpu" } else { "vulkan" }),
        "computeType": device.map(|_| "Q8_0"),
        "runtime": device.map(|_| format!("speech.cpp {VERSION} (native JSONL)")),
    })
}

async fn send(stdin: &mut ChildStdin, message: Value) -> Result<(), AsrError> {
    let mut encoded = serde_json::to_vec(&message).map_err(protocol_error)?;
    encoded.push(b'\n');
    tokio::time::timeout(REQUEST_WRITE_TIMEOUT, async {
        stdin.write_all(&encoded).await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| AsrError::Timeout)??;
    Ok(())
}

fn protocol_error(error: impl std::fmt::Display) -> AsrError {
    AsrError::Protocol(error.to_string())
}

fn worker_error(code: &str, message: &str) -> AsrError {
    AsrError::Worker(WorkerError {
        code: code.into(),
        message: message.into(),
    })
}

fn failure(id: u64, code: &str, message: &str) -> Value {
    json!({"id": id, "ok": false, "error": {"code": code, "message": message}})
}

#[cfg(all(test, windows))]
#[tokio::test]
#[ignore = "requires the installed pinned speech.cpp cache and a public PCM fixture"]
async fn native_speech_transcribes_through_the_desktop_supervisor() {
    use super::{JsonlTranscriber, Transcriber};
    use std::time::Duration;

    let settings = crate::types::Settings {
        asr_backend: "speech-cpp".into(),
        model_id: Some(MODEL_ID.into()),
        model_quantization: "8bit".into(),
        offline_only: true,
        ..Default::default()
    };
    let command = command(&settings).expect("select the opt-in native route");
    let worker = JsonlTranscriber::new(command, Duration::from_secs(15), Duration::from_secs(60));
    assert!(!worker.health().await.unwrap().loaded);
    worker.load("8bit").await.unwrap();
    let health = worker.health().await.unwrap();
    assert!(health.loaded);
    assert_eq!(health.compute_type.as_deref(), Some("Q8_0"));
    let wav = std::env::var_os("ASR_NATIVE_SPEECH_WAV")
        .map(PathBuf::from)
        .unwrap();
    for _ in 0..2 {
        let (_sender, cancel) = tokio::sync::watch::channel(false);
        let result = worker
            .transcribe_with_locale(&wav, None, Some("ja-JP"), cancel)
            .await
            .unwrap();
        assert!(!result.text.trim().is_empty());
        assert_eq!(result.model, "speech.cpp:haas-ja-v1.0:Q8_0");
    }

    // Start a long request and cancel after the complete transcribe frame was
    // sent. If the peer cannot acknowledge cancellation during decoding,
    // the normal supervisor drain must reap it and allow the next take.
    let root = tempfile::tempdir().unwrap();
    let long_clip = root.path().join("cancel.wav");
    let (mut data, _) = crate::audio::read_pcm16_wav_file(&wav).unwrap();
    let header_bytes = WAV_HEADER_BYTES as usize;
    let pcm = data[header_bytes..].repeat(20);
    data.truncate(header_bytes);
    let pcm_bytes = u32::try_from(pcm.len()).unwrap();
    data[4..8].copy_from_slice(&(36 + pcm_bytes).to_le_bytes());
    data[40..44].copy_from_slice(&pcm_bytes.to_le_bytes());
    data.extend_from_slice(&pcm);
    fs::write(&long_clip, data).unwrap();

    let mut state = worker.state.clone().lock_owned().await;
    let running = state.running.as_mut().unwrap();
    let pid = running.child.id().unwrap();
    let id = worker.next_id();
    running
        .send(&json!({
            "id": id,
            "command": "transcribe",
            "audio_path": long_clip,
            "language": "ja",
        }))
        .await
        .unwrap();
    // Let decoding begin on the real worker before exercising its cancel path.
    tokio::time::sleep(Duration::from_millis(150)).await;
    worker.drain_in_background(state, json!(id), worker.interrupt.subscribe());
    let loaded = tokio::time::timeout(Duration::from_secs(10), worker.health())
        .await
        .unwrap()
        .unwrap()
        .loaded;
    if loaded {
        eprintln!("native cancel: worker drained");
    } else {
        assert!(!crate::process_is_live(pid));
        eprintln!("native cancel: unresponsive worker reaped");
    }
    let (_sender, cancel) = tokio::sync::watch::channel(false);
    let result = worker
        .transcribe_with_locale(&wav, None, Some("ja-JP"), cancel)
        .await
        .unwrap();
    assert!(!result.text.trim().is_empty());
    eprintln!("native cancel: next recognition succeeded");

    worker.shutdown().await.unwrap();
    assert!(!worker.health().await.unwrap().loaded);
}

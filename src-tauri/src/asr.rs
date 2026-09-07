use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, watch, Mutex};
use tokio::time::timeout;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Progress notifications are advisory, so a slow subscriber may drop them
/// rather than delay the load it is reporting on.
const PROGRESS_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl WorkerCommand {
    pub fn python(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: vec!["-m".into(), "asr_worker".into()],
            // Windows otherwise uses the active ANSI code page for redirected
            // Python stdio (CP932 on Japanese systems).  The worker protocol is
            // UTF-8, so make that contract explicit for both directions.
            env: vec![
                ("PYTHONUTF8".into(), "1".into()),
                ("PYTHONIOENCODING".into(), "utf-8".into()),
                // The worker runs from the repository while the desktop binary
                // is built separately, so an app built before progress
                // notifications existed can spawn a worker that emits them.
                // Notifications are sent only to a client that asks for them.
                ("ASR_WORKER_PROGRESS".into(), "1".into()),
            ],
        }
    }

    pub fn with_backend(mut self, backend: &str) -> Self {
        self.args.push("--backend".into());
        self.args.push(backend.to_owned());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub speaker: Option<Value>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub segments: Vec<Segment>,
    pub model: String,
    pub duration_ms: u64,
}

/// Progress the worker reports while a `load` request is still running.
///
/// `stage` is `download` while model files are being fetched on first use and
/// `load` once cached files are read into memory. Byte counts are absent until
/// the download size is known, and for backends that cannot measure it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all(serialize = "camelCase"))]
pub struct LoadProgress {
    pub stage: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub completed_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorkerError {
    pub code: String,
    pub message: String,
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AsrError {
    #[error("worker I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("worker protocol failed: {0}")]
    Protocol(String),
    #[error("worker request timed out")]
    Timeout,
    #[error("worker exited unexpectedly")]
    Crashed,
    #[error("worker request cancelled")]
    Cancelled,
    #[error("worker error: {0}")]
    Worker(WorkerError),
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn load(&self, quantization: &str) -> Result<(), AsrError>;
    async fn transcribe(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError>;
    async fn shutdown(&self) -> Result<(), AsrError>;
    /// Reconfigure the worker command (for example when the ASR backend
    /// setting changes) and tear down any running worker so the next request
    /// spawns with the new command.
    async fn reconfigure(&self, command: WorkerCommand);
    /// Subscribe to the progress the worker reports during a model load.
    fn load_progress(&self) -> Option<broadcast::Receiver<LoadProgress>> {
        None
    }
}

struct RunningWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

struct State {
    command: WorkerCommand,
    running: Option<RunningWorker>,
    loaded_quantization: Option<String>,
}

pub struct JsonlTranscriber {
    request_timeout: Duration,
    load_timeout: Duration,
    next_id: AtomicU64,
    state: Arc<Mutex<State>>,
    progress: broadcast::Sender<LoadProgress>,
}

impl JsonlTranscriber {
    pub fn new(command: WorkerCommand, request_timeout: Duration, load_timeout: Duration) -> Self {
        Self {
            request_timeout,
            load_timeout,
            next_id: AtomicU64::new(1),
            state: Arc::new(Mutex::new(State {
                command,
                running: None,
                loaded_quantization: None,
            })),
            progress: broadcast::channel(PROGRESS_CHANNEL_CAPACITY).0,
        }
    }

    async fn spawn(&self, command_spec: &WorkerCommand) -> Result<RunningWorker, AsrError> {
        let mut command = Command::new(&command_spec.program);
        command
            .args(&command_spec.args)
            .envs(command_spec.env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AsrError::Protocol("worker stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AsrError::Protocol("worker stdout unavailable".into()))?;
        Ok(RunningWorker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    async fn exchange(
        &self,
        worker: &mut RunningWorker,
        request: &Value,
        response_timeout: Duration,
    ) -> Result<Value, AsrError> {
        let expected_id = request.get("id").cloned().unwrap_or(Value::Null);
        let mut encoded =
            serde_json::to_vec(request).map_err(|error| AsrError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        worker.stdin.write_all(&encoded).await?;
        worker.stdin.flush().await?;

        // A request may be preceded by progress notifications carrying the same
        // id; the response is the first line that reports an outcome.
        loop {
            let mut line = Vec::new();
            let read = timeout(
                response_timeout,
                (&mut worker.stdout)
                    .take((MAX_RESPONSE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line),
            )
            .await
            .map_err(|_| AsrError::Timeout)??;
            if read == 0 {
                let _ = worker.child.wait().await;
                return Err(AsrError::Crashed);
            }
            if line.len() > MAX_RESPONSE_BYTES || !line.ends_with(b"\n") {
                return Err(AsrError::Protocol(
                    "worker response exceeded size limit".into(),
                ));
            }
            match Self::parse_progress(&line, &expected_id) {
                Some(progress) => {
                    let _ = self.progress.send(progress);
                }
                None => return Self::validate_response(&line, &expected_id),
            }
        }
    }

    /// Recognize a progress notification for the request being awaited.
    ///
    /// Notifications repeat the request id and carry `event` instead of `ok`,
    /// so anything else — including a malformed notification — is handed to
    /// response validation and reported as a protocol error.
    fn parse_progress(line: &[u8], expected_id: &Value) -> Option<LoadProgress> {
        let sanitized = Self::sanitize_response_unicode(line);
        let message: Value = serde_json::from_slice(&sanitized).ok()?;
        if message.get("ok").is_some()
            || message.get("event") != Some(&json!("progress"))
            || message.get("id") != Some(expected_id)
        {
            return None;
        }
        serde_json::from_value(message).ok()
    }

    fn validate_response(line: &[u8], expected_id: &Value) -> Result<Value, AsrError> {
        std::str::from_utf8(line)
            .map_err(|_| AsrError::Protocol("worker response was not valid UTF-8".into()))?;
        let sanitized = Self::sanitize_response_unicode(line);
        let response: Value = serde_json::from_slice(&sanitized)
            .map_err(|error| AsrError::Protocol(error.to_string()))?;
        if response.get("id") != Some(expected_id) {
            return Err(AsrError::Protocol(
                "response id did not match request id".into(),
            ));
        }
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let error =
                serde_json::from_value(response.get("error").cloned().unwrap_or(Value::Null))
                    .map_err(|error| AsrError::Protocol(error.to_string()))?;
            return Err(AsrError::Worker(error));
        }
        Ok(response)
    }

    /// Make a worker response valid Unicode without changing JSON structure.
    ///
    /// Python and some model tokenizers can represent lone UTF-16 surrogate
    /// code points. They are not Unicode scalar values, so serde_json correctly
    /// rejects `\uD800` through `\uDFFF` unless they form a valid pair. Keep
    /// valid pairs and escaped backslashes intact, replacing only lone escapes.
    fn sanitize_response_unicode(line: &[u8]) -> Vec<u8> {
        let decoded = String::from_utf8_lossy(line);
        let bytes = decoded.as_bytes();
        let mut sanitized = Vec::with_capacity(bytes.len());
        let mut index = 0;
        let mut in_string = false;

        while index < bytes.len() {
            let byte = bytes[index];
            if !in_string {
                sanitized.push(byte);
                if byte == b'"' {
                    in_string = true;
                }
                index += 1;
                continue;
            }
            if byte == b'"' {
                sanitized.push(byte);
                in_string = false;
                index += 1;
                continue;
            }
            if byte != b'\\' || index + 1 >= bytes.len() {
                sanitized.push(byte);
                index += 1;
                continue;
            }
            if bytes[index + 1] != b'u' {
                sanitized.extend_from_slice(&bytes[index..index + 2]);
                index += 2;
                continue;
            }

            let Some(code_point) = Self::parse_unicode_escape(bytes, index) else {
                sanitized.push(byte);
                index += 1;
                continue;
            };
            if (0xD800..=0xDBFF).contains(&code_point) {
                let paired = Self::parse_unicode_escape(bytes, index + 6)
                    .is_some_and(|low| (0xDC00..=0xDFFF).contains(&low));
                if paired {
                    sanitized.extend_from_slice(&bytes[index..index + 12]);
                    index += 12;
                    continue;
                }
            }
            if (0xD800..=0xDFFF).contains(&code_point) {
                sanitized.extend_from_slice("\u{FFFD}".as_bytes());
                index += 6;
                continue;
            }
            sanitized.extend_from_slice(&bytes[index..index + 6]);
            index += 6;
        }
        sanitized
    }

    fn parse_unicode_escape(bytes: &[u8], index: usize) -> Option<u16> {
        let digits = bytes.get(index..index + 6)?;
        if digits[0] != b'\\' || digits[1] != b'u' {
            return None;
        }
        let text = std::str::from_utf8(&digits[2..]).ok()?;
        u16::from_str_radix(text, 16).ok()
    }

    fn requires_reset(error: &AsrError) -> bool {
        matches!(
            error,
            AsrError::Io(_)
                | AsrError::Protocol(_)
                | AsrError::Crashed
                | AsrError::Timeout
                | AsrError::Cancelled
        )
    }

    async fn ensure_running(&self, state: &mut State) -> Result<(), AsrError> {
        if state.running.is_some() {
            return Ok(());
        }
        let mut worker = self.spawn(&state.command).await?;
        if let Some(quantization) = state.loaded_quantization.clone() {
            let request = json!({
                "id": self.next_id.fetch_add(1, Ordering::Relaxed),
                "command": "load",
                "quantization": quantization,
            });
            self.exchange(&mut worker, &request, self.load_timeout)
                .await?;
        }
        state.running = Some(worker);
        Ok(())
    }

    async fn reset_locked(state: &mut State) {
        if let Some(mut worker) = state.running.take() {
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
        }
    }

    async fn reset(&self) {
        let mut state = self.state.lock().await;
        Self::reset_locked(&mut state).await;
    }

    async fn request(
        &self,
        request: Value,
        mut cancel: Option<watch::Receiver<bool>>,
        response_timeout: Duration,
    ) -> Result<Value, AsrError> {
        let mut state = self.state.lock().await;
        self.ensure_running(&mut state).await?;
        let worker = state.running.as_mut().expect("worker was ensured");
        let result = if let Some(receiver) = cancel.as_mut() {
            if *receiver.borrow() {
                Err(AsrError::Cancelled)
            } else {
                tokio::select! {
                    result = self.exchange(worker, &request, response_timeout) => result,
                    changed = receiver.changed() => {
                        match changed {
                            Ok(()) if *receiver.borrow() => Err(AsrError::Cancelled),
                            _ => self.exchange(worker, &request, response_timeout).await,
                        }
                    }
                }
            }
        } else {
            self.exchange(worker, &request, response_timeout).await
        };
        if result.as_ref().is_err_and(Self::requires_reset) {
            Self::reset_locked(&mut state).await;
        }
        result
    }
}

#[async_trait]
impl Transcriber for JsonlTranscriber {
    async fn load(&self, quantization: &str) -> Result<(), AsrError> {
        let request = json!({
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "command": "load",
            "quantization": quantization,
        });
        self.request(request, None, self.load_timeout).await?;
        self.state.lock().await.loaded_quantization = Some(quantization.to_owned());
        Ok(())
    }

    async fn transcribe(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        let request = json!({
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "command": "transcribe",
            "audio_path": audio_path,
            "prompt": prompt,
        });
        let response = self
            .request(request, Some(cancel), self.request_timeout)
            .await?;
        match serde_json::from_value(response) {
            Ok(transcript) => Ok(transcript),
            Err(error) => {
                self.reset().await;
                Err(AsrError::Protocol(error.to_string()))
            }
        }
    }

    async fn shutdown(&self) -> Result<(), AsrError> {
        let mut state = self.state.lock().await;
        let Some(mut worker) = state.running.take() else {
            return Ok(());
        };
        let request = json!({
            "id": self.next_id.fetch_add(1, Ordering::Relaxed),
            "command": "shutdown",
        });
        let exchange = timeout(
            SHUTDOWN_TIMEOUT,
            self.exchange(&mut worker, &request, SHUTDOWN_TIMEOUT),
        )
        .await;
        let wait = timeout(SHUTDOWN_TIMEOUT, worker.child.wait()).await;
        if exchange.is_err() || wait.is_err() {
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
        }
        match exchange {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(AsrError::Timeout),
        }
    }

    fn load_progress(&self) -> Option<broadcast::Receiver<LoadProgress>> {
        Some(self.progress.subscribe())
    }

    async fn reconfigure(&self, command: WorkerCommand) {
        let mut state = self.state.lock().await;
        if state.command == command {
            return;
        }
        Self::reset_locked(&mut state).await;
        state.loaded_quantization = None;
        state.command = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_successful_transcript() {
        let value = json!({
            "id": 4,
            "ok": true,
            "text": "hello",
            "segments": [{"start": 0.0, "end": 1.0, "speaker": 0, "text": "hello"}],
            "model": "mock",
            "duration_ms": 12
        });
        let transcript: Transcript = serde_json::from_value(value).unwrap();
        assert_eq!(transcript.text, "hello");
        assert_eq!(transcript.segments.len(), 1);
    }

    #[test]
    fn worker_command_is_injectable() {
        let command = WorkerCommand {
            program: "custom-worker".into(),
            args: vec!["--mock".into()],
            env: vec![("TEST".into(), "1".into())],
        };
        let client = JsonlTranscriber::new(
            command.clone(),
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        let state = client.state.try_lock().unwrap();
        assert_eq!(state.command, command);
        assert_eq!(state.command.program, PathBuf::from("custom-worker"));
        assert_eq!(client.request_timeout, Duration::from_secs(1));
        assert_eq!(client.load_timeout, Duration::from_secs(2));
    }

    #[test]
    fn python_command_appends_backend_argument() {
        let command = WorkerCommand::python("python").with_backend("faster-whisper");
        assert_eq!(
            command.args,
            vec![
                "-m".to_string(),
                "asr_worker".to_string(),
                "--backend".to_string(),
                "faster-whisper".to_string(),
            ]
        );
        assert_eq!(
            command.env,
            vec![
                ("PYTHONUTF8".to_string(), "1".to_string()),
                ("PYTHONIOENCODING".to_string(), "utf-8".to_string()),
                ("ASR_WORKER_PROGRESS".to_string(), "1".to_string()),
            ]
        );
    }

    #[test]
    fn rejects_non_utf8_worker_response_instead_of_corrupting_text() {
        // "マ" encoded as CP932. This is the byte pattern that previously
        // became a replacement character followed by an ASCII byte.
        let response = b"{\"id\":1,\"ok\":true,\"text\":\"\x83\x7d\"}\n";
        let error = JsonlTranscriber::validate_response(response, &json!(1)).unwrap_err();

        assert!(matches!(error, AsrError::Protocol(_)));
        assert_eq!(
            error.to_string(),
            "worker protocol failed: worker response was not valid UTF-8"
        );
    }

    #[test]
    fn malformed_transcript_is_protocol_error() {
        let value = json!({"id": 1, "ok": true, "text": "missing fields"});
        let error = serde_json::from_value::<Transcript>(value).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn malformed_and_mismatched_responses_require_reset() {
        for result in [
            JsonlTranscriber::validate_response(b"not-json\n", &json!(1)),
            JsonlTranscriber::validate_response(
                br#"{"id":2,"ok":true,"text":"wrong request"}"#,
                &json!(1),
            ),
        ] {
            let error = result.unwrap_err();
            assert!(matches!(error, AsrError::Protocol(_)));
            assert!(JsonlTranscriber::requires_reset(&error));
        }
    }

    #[test]
    fn worker_reported_error_is_not_a_protocol_reset() {
        let error = JsonlTranscriber::validate_response(
            br#"{"id":1,"ok":false,"error":{"code":"model","message":"failed"}}"#,
            &json!(1),
        )
        .unwrap_err();
        assert!(matches!(error, AsrError::Worker(_)));
        assert!(!JsonlTranscriber::requires_reset(&error));
    }

    #[test]
    fn progress_notification_is_read_without_ending_the_request() {
        let progress = JsonlTranscriber::parse_progress(
            br#"{"id":1,"event":"progress","stage":"download","model":"repo","completed_bytes":10,"total_bytes":40}"#,
            &json!(1),
        )
        .unwrap();

        assert_eq!(progress.stage, "download");
        assert_eq!(progress.model.as_deref(), Some("repo"));
        assert_eq!(progress.completed_bytes, Some(10));
        assert_eq!(progress.total_bytes, Some(40));
    }

    #[test]
    fn real_worker_download_notifications_are_consumed() {
        // Lines captured from `python -m asr_worker` during a first-use download.
        for line in [
            br#"{"id":1,"event":"progress","stage":"download","model":"Systran/faster-whisper-tiny"}"#.as_slice(),
            br#"{"id":1,"event":"progress","stage":"download","model":"Systran/faster-whisper-tiny","completed_bytes":2249,"total_bytes":null}"#.as_slice(),
            br#"{"id":1,"event":"progress","stage":"download","model":"Systran/faster-whisper-tiny","completed_bytes":2665349,"total_bytes":75538270}"#.as_slice(),
            br#"{"id":1,"event":"progress","stage":"load"}"#.as_slice(),
        ] {
            assert!(JsonlTranscriber::parse_progress(line, &json!(1)).is_some());
        }
    }

    #[test]
    fn progress_without_byte_counts_is_accepted() {
        let progress = JsonlTranscriber::parse_progress(
            br#"{"id":1,"event":"progress","stage":"load"}"#,
            &json!(1),
        )
        .unwrap();

        assert_eq!(progress.stage, "load");
        assert_eq!(progress.completed_bytes, None);
    }

    #[test]
    fn responses_and_foreign_notifications_are_not_progress() {
        for line in [
            br#"{"id":1,"ok":true,"model":"m","quantization":"4bit"}"#.as_slice(),
            br#"{"id":2,"event":"progress","stage":"download"}"#.as_slice(),
            br#"{"id":1,"event":"progress"}"#.as_slice(),
        ] {
            assert!(JsonlTranscriber::parse_progress(line, &json!(1)).is_none());
        }
    }

    #[test]
    fn sanitizes_lone_surrogates_in_worker_response() {
        let response = JsonlTranscriber::validate_response(
            br#"{"id":1,"ok":true,"text":"bad:\ud800","paired":"\ud83d\ude00","literal":"\\udfff"}"#,
            &json!(1),
        )
        .unwrap();

        assert_eq!(response["text"], "bad:\u{FFFD}");
        assert_eq!(response["paired"], "\u{1F600}");
        assert_eq!(response["literal"], r"\udfff");
    }
}

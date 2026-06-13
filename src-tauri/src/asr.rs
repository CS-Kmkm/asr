use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{watch, Mutex};
use tokio::time::timeout;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

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
            env: Vec::new(),
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

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorkerError {
    pub code: String,
    pub message: String,
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
    #[error("worker error {0:?}")]
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
    next_id: AtomicU64,
    state: Arc<Mutex<State>>,
}

impl JsonlTranscriber {
    pub fn new(command: WorkerCommand, request_timeout: Duration) -> Self {
        Self {
            request_timeout,
            next_id: AtomicU64::new(1),
            state: Arc::new(Mutex::new(State {
                command,
                running: None,
                loaded_quantization: None,
            })),
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
    ) -> Result<Value, AsrError> {
        let expected_id = request.get("id").cloned().unwrap_or(Value::Null);
        let mut encoded =
            serde_json::to_vec(request).map_err(|error| AsrError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        worker.stdin.write_all(&encoded).await?;
        worker.stdin.flush().await?;

        let mut line = Vec::new();
        let read = timeout(
            self.request_timeout,
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
        Self::validate_response(&line, &expected_id)
    }

    fn validate_response(line: &[u8], expected_id: &Value) -> Result<Value, AsrError> {
        let response: Value =
            serde_json::from_slice(line).map_err(|error| AsrError::Protocol(error.to_string()))?;
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
            self.exchange(&mut worker, &request).await?;
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
    ) -> Result<Value, AsrError> {
        let mut state = self.state.lock().await;
        self.ensure_running(&mut state).await?;
        let worker = state.running.as_mut().expect("worker was ensured");
        let result = if let Some(receiver) = cancel.as_mut() {
            if *receiver.borrow() {
                Err(AsrError::Cancelled)
            } else {
                tokio::select! {
                    result = self.exchange(worker, &request) => result,
                    changed = receiver.changed() => {
                        match changed {
                            Ok(()) if *receiver.borrow() => Err(AsrError::Cancelled),
                            _ => self.exchange(worker, &request).await,
                        }
                    }
                }
            }
        } else {
            self.exchange(worker, &request).await
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
        self.request(request, None).await?;
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
        let response = self.request(request, Some(cancel)).await?;
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
        let exchange = self.exchange(&mut worker, &request).await;
        let wait = timeout(self.request_timeout, worker.child.wait()).await;
        if wait.is_err() {
            let _ = worker.child.kill().await;
            let _ = worker.child.wait().await;
        }
        exchange.map(|_| ())
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
        let client = JsonlTranscriber::new(command.clone(), Duration::from_secs(1));
        let state = client.state.try_lock().unwrap();
        assert_eq!(state.command, command);
        assert_eq!(state.command.program, PathBuf::from("custom-worker"));
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
}

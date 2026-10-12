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
use tokio::sync::{broadcast, watch, Mutex, OwnedMutexGuard};
use tokio::time::timeout;

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a cancelled transcription may take to confirm a protocol
/// `cancel` before its worker is killed instead (which costs a model reload
/// on the next request). Backends stop at their next segment or token.
const CANCEL_GRACE: Duration = Duration::from_secs(3);
/// Progress notifications are advisory, so a slow subscriber may drop them
/// rather than delay the load it is reporting on.
const PROGRESS_CHANNEL_CAPACITY: usize = 32;
/// The worker reports no progress while it transcribes, so the response
/// timeout bounds the whole inference. Long recordings (or slow CPU backends)
/// get this many seconds per second of audio instead of the fixed minimum.
const TRANSCRIBE_TIMEOUT_PER_AUDIO_SECOND: u32 = 4;
/// Canonical PCM WAV header size written by the audio pipeline.
const WAV_HEADER_BYTES: u64 = 44;

/// Response timeout for transcribing `audio` worth of speech: never below the
/// configured `minimum`, otherwise proportional to the recording length.
pub(crate) fn transcription_timeout(minimum: Duration, audio: Duration) -> Duration {
    minimum.max(audio.saturating_mul(TRANSCRIBE_TIMEOUT_PER_AUDIO_SECOND))
}

/// Audio length of a PCM WAV, from its header byte rate and the file size.
/// Anything that is not a readable RIFF/WAVE file yields `None`.
fn wav_duration(header: &[u8], file_len: u64) -> Option<Duration> {
    if header.len() < WAV_HEADER_BYTES as usize
        || &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
    {
        return None;
    }
    let byte_rate = u32::from_le_bytes(header[28..32].try_into().ok()?);
    if byte_rate == 0 {
        return None;
    }
    let audio_bytes = file_len.saturating_sub(WAV_HEADER_BYTES);
    Some(Duration::from_secs_f64(
        audio_bytes as f64 / f64::from(byte_rate),
    ))
}

fn wav_file_duration(path: &Path) -> Option<Duration> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let mut header = [0u8; WAV_HEADER_BYTES as usize];
    file.read_exact(&mut header).ok()?;
    wav_duration(&header, file_len)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Working directory for the worker. `python -m` puts it first on
    /// `sys.path`, so it must not be the inherited working directory.
    pub current_dir: Option<PathBuf>,
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
            current_dir: None,
        }
    }

    pub fn with_backend(mut self, backend: &str) -> Self {
        self.args.push("--backend".into());
        self.args.push(backend.to_owned());
        self
    }

    pub fn with_current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(directory.into());
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
    /// `load` that gives up with `AsrError::Cancelled` once `cancel` turns
    /// true, for example when the recording that needs the model is cancelled.
    async fn load_cancellable(
        &self,
        quantization: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), AsrError> {
        let _ = cancel;
        self.load(quantization).await
    }
    /// `load`, optionally cancellable, that calls `started` once the load
    /// has the worker to itself (any load ahead of it has finished). A load
    /// cancelled while still waiting returns without calling it.
    async fn load_when_started(
        &self,
        quantization: &str,
        cancel: Option<watch::Receiver<bool>>,
        started: &(dyn Fn() + Send + Sync),
    ) -> Result<(), AsrError> {
        started();
        match cancel {
            Some(cancel) => self.load_cancellable(quantization, cancel).await,
            None => self.load(quantization).await,
        }
    }
    async fn transcribe(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError>;
    async fn transcribe_with_locale(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        locale: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        let _ = locale;
        self.transcribe(audio_path, prompt, cancel).await
    }
    async fn shutdown(&self) -> Result<(), AsrError>;
    /// Reconfigure the worker command (for example when the ASR backend
    /// setting changes) and tear down any running worker so the next request
    /// spawns with the new command. A request in flight, such as a model
    /// download, is abandoned instead of delaying the change. Reconfiguring
    /// with the current command changes nothing and interrupts nothing.
    async fn reconfigure(&self, command: WorkerCommand);
    /// Subscribe to the progress the worker reports during a model load.
    fn load_progress(&self) -> Option<broadcast::Receiver<LoadProgress>> {
        None
    }
    /// Whether the loaded model is in memory (see `ModelResidency`). A
    /// transcriber that does not track this returns `None`.
    fn model_residency(&self) -> Option<watch::Receiver<ModelResidency>> {
        None
    }
}

/// What the worker holds, as seen by the desktop's model status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelResidency {
    /// No model: nothing loaded yet, a failed load, or a reset worker.
    Absent,
    /// A respawned worker is reloading the model it held before a reset.
    /// The worker's progress notifications are relayed meanwhile.
    Restoring,
    /// Loaded; every successful load or restore gets a new generation.
    Resident(u64),
}

struct RunningWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// The start of a line whose read was abandoned by a cancelled wait. The
    /// next read continues the same line, so giving up on a response never
    /// desynchronizes the stream.
    partial_line: Vec<u8>,
    /// A request was sent and its response not yet read. Still set when the
    /// waiting future was dropped (for example an aborted startup load): the
    /// worker is busy and its stream holds a stale response, so it is killed
    /// rather than reused or asked to shut down.
    in_flight: bool,
}

impl RunningWorker {
    async fn send(&mut self, message: &Value) -> Result<(), AsrError> {
        let mut encoded =
            serde_json::to_vec(message).map_err(|error| AsrError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        self.in_flight = true;
        self.stdin.write_all(&encoded).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Read one complete line. Cancel-safe: a read dropped midway keeps what
    /// it consumed in `partial_line`.
    async fn read_line(&mut self, line_timeout: Duration) -> Result<Vec<u8>, AsrError> {
        loop {
            let limit = (MAX_RESPONSE_BYTES + 1).saturating_sub(self.partial_line.len());
            let read = timeout(
                line_timeout,
                (&mut self.stdout)
                    .take(limit as u64)
                    .read_until(b'\n', &mut self.partial_line),
            )
            .await
            .map_err(|_| AsrError::Timeout)??;
            if self.partial_line.len() > MAX_RESPONSE_BYTES {
                self.partial_line.clear();
                return Err(AsrError::Protocol(
                    "worker response exceeded size limit".into(),
                ));
            }
            if self.partial_line.ends_with(b"\n") {
                return Ok(std::mem::take(&mut self.partial_line));
            }
            if read == 0 {
                let _ = self.child.wait().await;
                return Err(AsrError::Crashed);
            }
        }
    }

    async fn kill(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

struct State {
    command: WorkerCommand,
    running: Option<RunningWorker>,
    /// Quantization of the model the running worker holds (or held before a
    /// crash, so a respawn can restore it). `None` after a failed load.
    loaded_quantization: Option<String>,
    residency: watch::Sender<ModelResidency>,
    load_generation: u64,
}

impl State {
    fn mark_resident(&mut self) {
        self.load_generation += 1;
        self.residency
            .send_replace(ModelResidency::Resident(self.load_generation));
    }

    fn mark_not_resident(&self) {
        self.residency.send_if_modified(|residency| {
            let changed = *residency != ModelResidency::Absent;
            *residency = ModelResidency::Absent;
            changed
        });
    }

    /// Kill the worker. The next request spawns a new one and reloads the
    /// model it held.
    async fn reset(&mut self) {
        if let Some(mut worker) = self.running.take() {
            worker.kill().await;
        }
        self.mark_not_resident();
    }

    /// Kill a worker whose last response was abandoned without a drain. A
    /// restore abandoned the same way left no worker behind.
    async fn discard_abandoned_worker(&mut self) {
        match &self.running {
            Some(worker) if worker.in_flight => self.reset().await,
            Some(_) => {}
            None => self.mark_not_resident(),
        }
    }
}

/// Why a wait for the worker ended without its response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interruption {
    /// The request's own cancel signal turned true.
    Cancelled,
    /// `shutdown` or `reconfigure` needs the worker.
    Interrupted,
}

fn cancel_requested(cancel: &Option<watch::Receiver<bool>>) -> bool {
    cancel.as_ref().is_some_and(|receiver| *receiver.borrow())
}

async fn cancel_changed(
    cancel: Option<&mut watch::Receiver<bool>>,
) -> Result<(), watch::error::RecvError> {
    match cancel {
        Some(receiver) => receiver.changed().await,
        None => std::future::pending().await,
    }
}

/// Await `operation` unless the request is cancelled or the transcriber is
/// interrupted first.
///
/// A dropped cancel sender only means nobody can cancel any more: the wait
/// continues on the same future, so the request is never sent twice.
async fn until_cancelled<T>(
    operation: impl std::future::Future<Output = T>,
    cancel: &mut Option<watch::Receiver<bool>>,
    interrupt: &mut watch::Receiver<u64>,
) -> Result<T, Interruption> {
    if cancel_requested(cancel) {
        return Err(Interruption::Cancelled);
    }
    tokio::pin!(operation);
    loop {
        tokio::select! {
            output = &mut operation => return Ok(output),
            _ = interrupt.changed() => return Err(Interruption::Interrupted),
            changed = cancel_changed(cancel.as_mut()) => match changed {
                Ok(()) if cancel_requested(cancel) => return Err(Interruption::Cancelled),
                Ok(()) => {}
                Err(_) => *cancel = None,
            },
        }
    }
}

/// Ask the worker to cancel request `target_id`, then consume its reply and
/// the cancel's own reply, in either order.
///
/// A worker that predates `cancel` keeps transcribing and answers the cancel
/// with `unsupported_operation` afterwards, so it is drained the same way as
/// long as it finishes within the caller's grace period.
async fn drain_cancelled(
    worker: &mut RunningWorker,
    target_id: &Value,
    cancel_id: u64,
    line_timeout: Duration,
) -> Result<(), AsrError> {
    worker
        .send(&json!({"id": cancel_id, "command": "cancel", "target_id": target_id}))
        .await?;
    let cancel_id = json!(cancel_id);
    let (mut target_answered, mut cancel_answered) = (false, false);
    while !(target_answered && cancel_answered) {
        let line = worker.read_line(line_timeout).await?;
        let message: Value =
            serde_json::from_slice(&JsonlTranscriber::sanitize_response_unicode(&line))
                .map_err(|error| AsrError::Protocol(error.to_string()))?;
        let id = message.get("id");
        if id == Some(target_id) {
            // Progress notifications for the target carry no outcome.
            target_answered |= message.get("ok").is_some();
        } else if id == Some(&cancel_id) {
            cancel_answered = true;
        } else {
            return Err(AsrError::Protocol(
                "unexpected worker message while cancelling".into(),
            ));
        }
    }
    worker.in_flight = false;
    Ok(())
}

pub struct JsonlTranscriber {
    request_timeout: Duration,
    load_timeout: Duration,
    cancel_grace: Duration,
    shutdown_timeout: Duration,
    next_id: AtomicU64,
    state: Arc<Mutex<State>>,
    progress: broadcast::Sender<LoadProgress>,
    /// Bumped by `shutdown` and `reconfigure` before they wait for the state
    /// lock, so whatever holds it (a model download, a cancelled request
    /// being drained) kills the worker and lets go instead of blocking them.
    interrupt: watch::Sender<u64>,
    residency: watch::Receiver<ModelResidency>,
    /// The command `State` runs, readable without the state lock. A running
    /// load holds that lock, so `reconfigure` compares against this copy to
    /// leave the load alone when the command is unchanged. Held through a
    /// whole reconfiguration, so concurrent ones apply in order and the copy
    /// never disagrees with `State::command` once they finish.
    configured_command: Mutex<WorkerCommand>,
}

/// Build the worker process command: the discovered working directory, no
/// console window, and stderr routed to the worker log (`log_stderr`),
/// inherited in debug builds, or discarded in release builds.
fn worker_process_command(command_spec: &WorkerCommand, log_stderr: bool) -> Command {
    let mut command = Command::new(&command_spec.program);
    command
        .args(&command_spec.args)
        .envs(command_spec.env.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(if log_stderr {
            Stdio::piped()
        } else if cfg!(debug_assertions) {
            Stdio::inherit()
        } else {
            // A GUI-subsystem parent has no stderr to inherit; Python would
            // then see sys.stderr as None, and diagnostics written to it
            // could end up on stdout and corrupt the JSONL protocol.
            Stdio::null()
        })
        .kill_on_drop(true);
    if let Some(directory) = &command_spec.current_dir {
        command.current_dir(directory);
    }
    // The release app has no console for the Python worker to share, so
    // it would otherwise open a console window of its own.
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

impl JsonlTranscriber {
    pub fn new(command: WorkerCommand, request_timeout: Duration, load_timeout: Duration) -> Self {
        let (residency, residency_receiver) = watch::channel(ModelResidency::Absent);
        Self {
            request_timeout,
            load_timeout,
            cancel_grace: CANCEL_GRACE,
            shutdown_timeout: SHUTDOWN_TIMEOUT,
            next_id: AtomicU64::new(1),
            configured_command: Mutex::new(command.clone()),
            state: Arc::new(Mutex::new(State {
                command,
                running: None,
                loaded_quantization: None,
                residency,
                load_generation: 0,
            })),
            progress: broadcast::channel(PROGRESS_CHANNEL_CAPACITY).0,
            interrupt: watch::channel(0).0,
            residency: residency_receiver,
        }
    }

    #[cfg(test)]
    fn with_grace_periods(mut self, cancel_grace: Duration, shutdown_timeout: Duration) -> Self {
        self.cancel_grace = cancel_grace;
        self.shutdown_timeout = shutdown_timeout;
        self
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn interrupt_running_request(&self) {
        self.interrupt
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    async fn spawn(&self, command_spec: &WorkerCommand) -> Result<RunningWorker, AsrError> {
        // stderr carries the worker's internal_error details; persist it to the
        // rotating worker log when the app installed one.
        let mut command = worker_process_command(command_spec, crate::worker_log::is_installed());
        let mut child = command.spawn()?;
        if let Some(stderr) = child.stderr.take() {
            crate::worker_log::mark_worker_start();
            tokio::spawn(crate::worker_log::pump(stderr));
        }
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
            partial_line: Vec::new(),
            in_flight: false,
        })
    }

    /// Read the response to `expected_id`, relaying the progress
    /// notifications that precede it.
    async fn read_response(
        &self,
        worker: &mut RunningWorker,
        expected_id: &Value,
        response_timeout: Duration,
    ) -> Result<Value, AsrError> {
        // A request may be preceded by progress notifications carrying the same
        // id; the response is the first line that reports an outcome.
        loop {
            let line = worker.read_line(response_timeout).await?;
            match Self::parse_progress(&line, expected_id) {
                Some(progress) => {
                    let _ = self.progress.send(progress);
                }
                None => {
                    worker.in_flight = false;
                    return Self::validate_response(&line, expected_id);
                }
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

    /// Errors after which the worker's stream or process can no longer be
    /// trusted. Cancellation is not one: a cancelled request is drained.
    fn requires_reset(error: &AsrError) -> bool {
        matches!(
            error,
            AsrError::Io(_) | AsrError::Protocol(_) | AsrError::Crashed | AsrError::Timeout
        )
    }

    /// Take the state lock unless the request is cancelled, or `shutdown` or
    /// `reconfigure` takes over, while it waits (for example behind a model
    /// download).
    async fn lock_state(
        &self,
        cancel: &mut Option<watch::Receiver<bool>>,
        interrupt: &mut watch::Receiver<u64>,
    ) -> Result<OwnedMutexGuard<State>, AsrError> {
        let state = until_cancelled(self.state.clone().lock_owned(), cancel, interrupt)
            .await
            .map_err(|_| AsrError::Cancelled)?;
        let mut state = state;
        if interrupt.has_changed().unwrap_or(true) || cancel_requested(cancel) {
            return Err(AsrError::Cancelled);
        }
        state.discard_abandoned_worker().await;
        Ok(state)
    }

    /// Spawn a worker when none is running and restore the model the previous
    /// one held. The restore can be cancelled like the request that needs it.
    async fn ensure_running(
        &self,
        state: &mut State,
        cancel: &mut Option<watch::Receiver<bool>>,
        interrupt: &mut watch::Receiver<u64>,
    ) -> Result<(), AsrError> {
        if state.running.is_some() {
            return Ok(());
        }
        let mut worker = self.spawn(&state.command).await?;
        if let Some(quantization) = state.loaded_quantization.clone() {
            // The desktop shows this restore like a load, with its progress.
            state.residency.send_replace(ModelResidency::Restoring);
            let id = self.next_id();
            let restored = match worker.send(&load_request(id, &quantization)).await {
                Ok(()) => until_cancelled(
                    self.read_response(&mut worker, &json!(id), self.load_timeout),
                    cancel,
                    interrupt,
                )
                .await
                .unwrap_or(Err(AsrError::Cancelled)),
                Err(error) => Err(error),
            };
            if let Err(error) = restored {
                worker.kill().await;
                state.mark_not_resident();
                return Err(error);
            }
            state.mark_resident();
        }
        state.running = Some(worker);
        Ok(())
    }

    async fn load_with(
        &self,
        quantization: &str,
        mut cancel: Option<watch::Receiver<bool>>,
        started: Option<&(dyn Fn() + Send + Sync)>,
    ) -> Result<(), AsrError> {
        let mut interrupt = self.interrupt.subscribe();
        let mut state = self.lock_state(&mut cancel, &mut interrupt).await?;
        if let Some(started) = started {
            started();
        }
        if state.running.is_some() && state.loaded_quantization.as_deref() == Some(quantization) {
            // Already in memory, for example restored after a worker restart.
            return Ok(());
        }
        if state.running.is_none() {
            // This load replaces whatever a respawn would have restored.
            state.running = Some(self.spawn(&state.command).await?);
        }
        let id = self.next_id();
        let worker = state.running.as_mut().expect("worker was spawned");
        let result = match worker.send(&load_request(id, quantization)).await {
            Ok(()) => {
                until_cancelled(
                    self.read_response(worker, &json!(id), self.load_timeout),
                    &mut cancel,
                    &mut interrupt,
                )
                .await
            }
            Err(error) => Ok(Err(error)),
        };
        match result {
            Ok(Ok(_)) => {
                state.loaded_quantization = Some(quantization.to_owned());
                state.mark_resident();
                Ok(())
            }
            Ok(Err(error)) => {
                // The worker unloads its previous model before loading.
                state.loaded_quantization = None;
                if Self::requires_reset(&error) {
                    state.reset().await;
                } else {
                    state.mark_not_resident();
                }
                Err(error)
            }
            Err(_) => {
                // A load has no safe point to stop at, so the worker goes.
                state.loaded_quantization = None;
                state.reset().await;
                Err(AsrError::Cancelled)
            }
        }
    }

    async fn transcribe_request(
        &self,
        request: Value,
        cancel: watch::Receiver<bool>,
        response_timeout: Duration,
    ) -> Result<Value, AsrError> {
        let mut cancel = Some(cancel);
        let mut interrupt = self.interrupt.subscribe();
        let mut state = self.lock_state(&mut cancel, &mut interrupt).await?;
        self.ensure_running(&mut state, &mut cancel, &mut interrupt)
            .await?;
        let target_id = request.get("id").cloned().unwrap_or(Value::Null);
        let worker = state.running.as_mut().expect("worker was ensured");
        let outcome = match worker.send(&request).await {
            Ok(()) => {
                until_cancelled(
                    self.read_response(worker, &target_id, response_timeout),
                    &mut cancel,
                    &mut interrupt,
                )
                .await
            }
            Err(error) => Ok(Err(error)),
        };
        match outcome {
            Ok(result) => {
                if result.as_ref().is_err_and(Self::requires_reset) {
                    state.reset().await;
                }
                result
            }
            Err(Interruption::Interrupted) => {
                state.reset().await;
                Err(AsrError::Cancelled)
            }
            Err(Interruption::Cancelled) => {
                self.drain_in_background(state, target_id, interrupt);
                Err(AsrError::Cancelled)
            }
        }
    }

    /// Cancel the request in flight without delaying the caller, and keep the
    /// worker (and its loaded model) when it confirms within the grace period.
    /// The state lock travels with the drain, so the next request waits for
    /// it instead of reading the abandoned response.
    fn drain_in_background(
        &self,
        mut state: OwnedMutexGuard<State>,
        target_id: Value,
        mut interrupt: watch::Receiver<u64>,
    ) {
        let cancel_id = self.next_id();
        let grace = self.cancel_grace;
        tokio::spawn(async move {
            let drained = match state.running.as_mut() {
                Some(worker) => tokio::select! {
                    drained = timeout(grace, drain_cancelled(worker, &target_id, cancel_id, grace)) => {
                        matches!(drained, Ok(Ok(())))
                    }
                    _ = interrupt.changed() => false,
                },
                None => true,
            };
            if !drained {
                state.reset().await;
            }
        });
    }

    async fn reset(&self) {
        self.state.lock().await.reset().await;
    }
}

fn load_request(id: u64, quantization: &str) -> Value {
    json!({
        "id": id,
        "command": "load",
        "quantization": quantization,
    })
}

/// JSONL `transcribe` request. `language` carries the full speech locale tag
/// (or null for automatic detection); the worker backend decides how much of
/// it to honor.
fn transcribe_request(
    id: u64,
    audio_path: &Path,
    prompt: Option<&str>,
    locale: Option<&str>,
) -> Value {
    json!({
        "id": id,
        "command": "transcribe",
        "audio_path": audio_path,
        "prompt": prompt,
        "language": locale,
    })
}

#[async_trait]
impl Transcriber for JsonlTranscriber {
    async fn load(&self, quantization: &str) -> Result<(), AsrError> {
        self.load_with(quantization, None, None).await
    }

    async fn load_cancellable(
        &self,
        quantization: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), AsrError> {
        self.load_with(quantization, Some(cancel), None).await
    }

    async fn load_when_started(
        &self,
        quantization: &str,
        cancel: Option<watch::Receiver<bool>>,
        started: &(dyn Fn() + Send + Sync),
    ) -> Result<(), AsrError> {
        self.load_with(quantization, cancel, Some(started)).await
    }

    async fn transcribe(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        self.transcribe_with_locale(audio_path, prompt, None, cancel)
            .await
    }

    async fn transcribe_with_locale(
        &self,
        audio_path: &Path,
        prompt: Option<&str>,
        locale: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        let request = transcribe_request(self.next_id(), audio_path, prompt, locale);
        let response_timeout = transcription_timeout(
            self.request_timeout,
            wav_file_duration(audio_path).unwrap_or_default(),
        );
        let response = self
            .transcribe_request(request, cancel, response_timeout)
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
        self.interrupt_running_request();
        let mut state = self.state.lock().await;
        state.discard_abandoned_worker().await;
        state.mark_not_resident();
        let Some(mut worker) = state.running.take() else {
            return Ok(());
        };
        let id = self.next_id();
        let exchange = timeout(self.shutdown_timeout, async {
            worker
                .send(&json!({"id": id, "command": "shutdown"}))
                .await?;
            self.read_response(&mut worker, &json!(id), self.shutdown_timeout)
                .await
        })
        .await;
        // A worker that did not answer is not waited for a second time.
        let exited = exchange.is_ok()
            && timeout(self.shutdown_timeout, worker.child.wait())
                .await
                .is_ok();
        if !exited {
            worker.kill().await;
        }
        match exchange {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(AsrError::Timeout),
        }
    }

    fn load_progress(&self) -> Option<broadcast::Receiver<LoadProgress>> {
        Some(self.progress.subscribe())
    }

    fn model_residency(&self) -> Option<watch::Receiver<ModelResidency>> {
        Some(self.residency.clone())
    }

    async fn reconfigure(&self, command: WorkerCommand) {
        let mut configured = self.configured_command.lock().await;
        // An unchanged command keeps the worker, its loaded model and any
        // request in flight (such as a model download).
        if *configured == command {
            return;
        }
        self.interrupt_running_request();
        let mut state = self.state.lock().await;
        if state.command != command {
            state.reset().await;
            state.loaded_quantization = None;
            state.command = command.clone();
        }
        *configured = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcription_timeout_scales_with_long_recordings_only() {
        let minimum = Duration::from_secs(300);
        assert_eq!(transcription_timeout(minimum, Duration::ZERO), minimum);
        assert_eq!(
            transcription_timeout(minimum, Duration::from_secs(60)),
            minimum
        );
        assert_eq!(
            transcription_timeout(minimum, Duration::from_secs(75)),
            minimum
        );
        assert_eq!(
            transcription_timeout(minimum, Duration::from_secs(15 * 60)),
            Duration::from_secs(60 * 60)
        );
        assert_eq!(transcription_timeout(minimum, Duration::MAX), Duration::MAX);
    }

    fn wav_header(byte_rate: u32) -> Vec<u8> {
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16u32.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&1u16.to_le_bytes());
        header.extend_from_slice(&(byte_rate / 2).to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&2u16.to_le_bytes());
        header.extend_from_slice(&16u16.to_le_bytes());
        header.extend_from_slice(b"data");
        header.extend_from_slice(&0u32.to_le_bytes());
        header
    }

    #[test]
    fn wav_duration_reads_the_recording_length() {
        // 24 kHz mono PCM16: 48,000 bytes per second.
        let header = wav_header(48_000);
        assert_eq!(
            wav_duration(&header, 44 + 48_000 * 600),
            Some(Duration::from_secs(600))
        );
        assert_eq!(wav_duration(&header, 10), Some(Duration::ZERO));
        assert_eq!(wav_duration(&wav_header(0), 44 + 48_000), None);
        assert_eq!(wav_duration(b"not a wav file", 1_000), None);
        let mut not_wave = header.clone();
        not_wave[8..12].copy_from_slice(b"AVI ");
        assert_eq!(wav_duration(&not_wave, 44 + 48_000), None);
    }

    #[test]
    fn wav_file_duration_is_unknown_for_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            wav_file_duration(&directory.path().join("missing.wav")),
            None
        );
        let path = directory.path().join("take.wav");
        let mut bytes = wav_header(48_000);
        bytes.resize(44 + 48_000 * 2, 0);
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(wav_file_duration(&path), Some(Duration::from_secs(2)));
    }

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
    fn transcribe_request_carries_the_speech_locale() {
        let path = Path::new("recording.wav");
        let request = transcribe_request(7, path, Some("terms"), Some("en-GB"));
        assert_eq!(request["command"], "transcribe");
        assert_eq!(request["language"], "en-GB");
        assert_eq!(request["prompt"], "terms");
        let automatic = transcribe_request(8, path, None, None);
        assert!(automatic
            .as_object()
            .unwrap()
            .get("language")
            .unwrap()
            .is_null());
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
        assert_eq!(command.current_dir, None);
        assert_eq!(
            command.with_current_dir("checkout").current_dir,
            Some(PathBuf::from("checkout"))
        );
    }

    #[test]
    fn worker_process_keeps_its_directory_with_or_without_the_log_pipe() {
        let spec = WorkerCommand::python("python").with_current_dir("checkout");
        for log_stderr in [false, true] {
            let command = worker_process_command(&spec, log_stderr);
            let command = command.as_std();
            assert_eq!(command.get_program(), "python");
            assert_eq!(command.get_current_dir(), Some(Path::new("checkout")));
        }
        let without_directory = worker_process_command(&WorkerCommand::python("python"), true);
        assert_eq!(without_directory.as_std().get_current_dir(), None);
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

    #[tokio::test]
    async fn reconfigure_interrupts_only_for_a_new_command() {
        let command = WorkerCommand::python("python").with_backend("mock");
        let transcriber = JsonlTranscriber::new(
            command.clone(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        let mut interrupt = transcriber.interrupt.subscribe();
        // Like a model load in progress: the state lock is taken.
        let load = transcriber.state.clone().lock_owned().await;
        timeout(
            Duration::from_secs(1),
            transcriber.reconfigure(command.clone()),
        )
        .await
        .expect("an unchanged command waited for the running load");
        assert!(!interrupt.has_changed().unwrap());
        drop(load);

        let changed = command.with_backend("faster-whisper");
        transcriber.reconfigure(changed.clone()).await;
        assert!(interrupt.has_changed().unwrap());
        interrupt.mark_unchanged();
        assert_eq!(transcriber.state.lock().await.command, changed);

        transcriber.reconfigure(changed.clone()).await;
        assert!(!interrupt.has_changed().unwrap());
    }

    /// `JsonlTranscriber` against the real `python -m asr_worker --backend
    /// mock` process. The mock never downloads a model; files in the test's
    /// directory hold or crash it on cue and it logs what it ran.
    mod worker_process {
        use super::*;
        use std::ffi::OsString;
        use std::time::Instant;

        const BOUNDED: Duration = Duration::from_secs(3);

        fn repository_root() -> PathBuf {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("src-tauri has a parent")
                .to_path_buf()
        }

        /// The interpreter the app would use (`ASR_PYTHON`, else the project
        /// virtual environment), else `python` on PATH. `None` skips the test.
        fn python() -> Option<OsString> {
            let root = repository_root();
            let venv = if cfg!(windows) {
                root.join(".venv").join("Scripts").join("python.exe")
            } else {
                root.join(".venv").join("bin").join("python")
            };
            let candidates = std::env::var_os("ASR_PYTHON")
                .into_iter()
                .chain(venv.is_file().then(|| venv.into_os_string()))
                .chain([OsString::from("python")]);
            for candidate in candidates {
                let supported = std::process::Command::new(&candidate)
                    .args(["-c", "import sys; sys.exit(sys.version_info < (3, 10))"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|status| status.success());
                if supported {
                    return Some(candidate);
                }
            }
            eprintln!("skipping worker process test: no Python 3.10+ found");
            None
        }

        struct MockWorker {
            directory: tempfile::TempDir,
            command: WorkerCommand,
            audio: PathBuf,
        }

        impl MockWorker {
            fn new(extra_env: &[(&str, &str)]) -> Option<Self> {
                let python = python()?;
                let directory = tempfile::tempdir().unwrap();
                let mut command = WorkerCommand::python(python).with_backend("mock");
                command.env.push((
                    "PYTHONPATH".into(),
                    repository_root().to_string_lossy().into_owned(),
                ));
                for (name, file) in [
                    ("ASR_WORKER_MOCK_EVENT_LOG", "events.log"),
                    ("ASR_WORKER_MOCK_HOLD_FILE", "hold"),
                    ("ASR_WORKER_MOCK_CRASH_FILE", "crash"),
                ] {
                    let path = directory.path().join(file);
                    command
                        .env
                        .push((name.into(), path.to_string_lossy().into_owned()));
                }
                for (name, value) in extra_env {
                    command.env.push(((*name).into(), (*value).into()));
                }
                let audio = directory.path().join("take.wav");
                let mut bytes = wav_header(48_000);
                bytes.resize(44 + 4_800, 0);
                std::fs::write(&audio, bytes).unwrap();
                Some(Self {
                    directory,
                    command,
                    audio,
                })
            }

            fn transcriber(&self) -> JsonlTranscriber {
                JsonlTranscriber::new(
                    self.command.clone(),
                    Duration::from_secs(20),
                    Duration::from_secs(20),
                )
                .with_grace_periods(Duration::from_secs(5), Duration::from_secs(2))
            }

            fn flag(&self, name: &str) -> PathBuf {
                self.directory.path().join(name)
            }

            fn hold(&self) {
                std::fs::write(self.flag("hold"), b"").unwrap();
            }

            fn release(&self) {
                std::fs::remove_file(self.flag("hold")).unwrap();
            }

            /// `(pid, event)` pairs in the order the mock logged them.
            fn events(&self) -> Vec<(String, String)> {
                std::fs::read_to_string(self.flag("events.log"))
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|line| line.split_once(' '))
                    .map(|(pid, event)| (pid.to_owned(), event.to_owned()))
                    .collect()
            }

            fn event_names(&self) -> Vec<String> {
                self.events().into_iter().map(|(_, event)| event).collect()
            }

            fn processes(&self) -> usize {
                let mut pids: Vec<_> = self.events().into_iter().map(|(pid, _)| pid).collect();
                pids.dedup();
                pids.len()
            }

            async fn wait_for_event(&self, event: &str, count: usize) {
                let deadline = Instant::now() + Duration::from_secs(20);
                while self
                    .event_names()
                    .iter()
                    .filter(|name| *name == event)
                    .count()
                    < count
                {
                    assert!(Instant::now() < deadline, "worker never logged {event}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }

            async fn transcribe(
                &self,
                transcriber: &JsonlTranscriber,
            ) -> Result<Transcript, AsrError> {
                let (_cancel, cancel) = watch::channel(false);
                transcriber.transcribe(&self.audio, None, cancel).await
            }
        }

        fn resident(transcriber: &JsonlTranscriber) -> Option<u64> {
            match *transcriber.model_residency().unwrap().borrow() {
                ModelResidency::Resident(generation) => Some(generation),
                ModelResidency::Absent | ModelResidency::Restoring => None,
            }
        }

        async fn wait_until_not_resident(transcriber: &JsonlTranscriber) {
            let mut residency = transcriber.model_residency().unwrap();
            timeout(
                Duration::from_secs(10),
                residency.wait_for(|residency| *residency == ModelResidency::Absent),
            )
            .await
            .expect("worker was never reset")
            .unwrap();
        }

        #[tokio::test]
        async fn loads_transcribes_and_shuts_down() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock.transcriber();
            transcriber.load("4bit").await.unwrap();
            assert_eq!(resident(&transcriber), Some(1));
            // Loading the resident quantization again needs no worker round trip.
            transcriber.load("4bit").await.unwrap();

            let transcript = mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(transcript.text, "mock transcription");
            transcriber.shutdown().await.unwrap();

            assert_eq!(resident(&transcriber), None);
            assert_eq!(mock.event_names(), ["load 4bit", "transcribe"]);
        }

        #[tokio::test]
        async fn a_crash_respawns_the_worker_and_restores_the_model() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock.transcriber();
            transcriber.load("8bit").await.unwrap();
            std::fs::write(mock.flag("crash"), b"").unwrap();

            let crashed = mock.transcribe(&transcriber).await;
            assert!(matches!(crashed, Err(AsrError::Crashed)), "{crashed:?}");
            assert_eq!(
                *transcriber.model_residency().unwrap().borrow(),
                ModelResidency::Absent
            );

            let mut residency = transcriber.model_residency().unwrap();
            let observed = tokio::spawn(async move {
                let mut seen = Vec::new();
                while residency.changed().await.is_ok() {
                    let current = *residency.borrow_and_update();
                    seen.push(current);
                    if matches!(current, ModelResidency::Resident(_)) {
                        break;
                    }
                }
                seen
            });
            let transcript = mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(transcript.text, "mock transcription");
            assert!(resident(&transcriber).is_some());
            assert_eq!(
                observed.await.unwrap(),
                [ModelResidency::Restoring, ModelResidency::Resident(2)]
            );
            assert_eq!(
                mock.event_names(),
                ["load 8bit", "transcribe", "load 8bit", "transcribe"]
            );
            assert_eq!(mock.processes(), 2);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn a_timeout_resets_the_worker_within_the_timeout() {
            let Some(mock) = MockWorker::new(&[("ASR_WORKER_MOCK_IGNORE_CANCEL", "1")]) else {
                return;
            };
            let transcriber = JsonlTranscriber::new(
                mock.command.clone(),
                Duration::from_millis(500),
                Duration::from_secs(20),
            );
            transcriber.load("4bit").await.unwrap();
            mock.hold();

            let started = Instant::now();
            let result = mock.transcribe(&transcriber).await;
            assert!(matches!(result, Err(AsrError::Timeout)), "{result:?}");
            assert!(started.elapsed() < BOUNDED);
            assert_eq!(resident(&transcriber), None);
            mock.release();
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn cancelling_a_transcription_keeps_the_worker_and_its_model() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock.transcriber();
            transcriber.load("4bit").await.unwrap();
            let generation = resident(&transcriber);
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let request = transcriber.transcribe(&mock.audio, None, cancel);
            let cancel_once_running = async {
                mock.wait_for_event("transcribe", 1).await;
                cancel_tx.send_replace(true);
                Instant::now()
            };
            let (result, cancelled_at) = tokio::join!(request, cancel_once_running);
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            assert!(cancelled_at.elapsed() < BOUNDED);

            // The held backend stops only because the worker relayed the cancel.
            mock.wait_for_event("cancelled", 1).await;
            mock.release();
            let transcript = mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(transcript.text, "mock transcription");
            assert_eq!(resident(&transcriber), generation);
            assert_eq!(
                mock.event_names(),
                ["load 4bit", "transcribe", "cancelled", "transcribe"]
            );
            assert_eq!(mock.processes(), 1);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn a_cancelled_backend_that_finishes_within_the_grace_period_is_kept() {
            // Like a backend that cannot stop early, or a worker that predates
            // `cancel`: the abandoned response is drained when it arrives.
            let Some(mock) = MockWorker::new(&[("ASR_WORKER_MOCK_IGNORE_CANCEL", "1")]) else {
                return;
            };
            let transcriber = mock.transcriber();
            transcriber.load("4bit").await.unwrap();
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let request = transcriber.transcribe(&mock.audio, None, cancel);
            let cancel_once_running = async {
                mock.wait_for_event("transcribe", 1).await;
                cancel_tx.send_replace(true);
            };
            let (result, ()) = tokio::join!(request, cancel_once_running);
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            mock.release();

            mock.transcribe(&transcriber).await.unwrap();
            assert!(resident(&transcriber).is_some());
            assert_eq!(
                mock.event_names(),
                ["load 4bit", "transcribe", "transcribe"]
            );
            assert_eq!(mock.processes(), 1);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn an_unconfirmed_cancel_falls_back_to_killing_the_worker() {
            let Some(mock) = MockWorker::new(&[("ASR_WORKER_MOCK_IGNORE_CANCEL", "1")]) else {
                return;
            };
            let transcriber = mock
                .transcriber()
                .with_grace_periods(Duration::from_millis(300), Duration::from_secs(2));
            transcriber.load("4bit").await.unwrap();
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let request = transcriber.transcribe(&mock.audio, None, cancel);
            let cancel_once_running = async {
                mock.wait_for_event("transcribe", 1).await;
                cancel_tx.send_replace(true);
            };
            let (result, ()) = tokio::join!(request, cancel_once_running);
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            wait_until_not_resident(&transcriber).await;
            mock.release();

            mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(
                mock.event_names(),
                ["load 4bit", "transcribe", "load 4bit", "transcribe"]
            );
            assert_eq!(mock.processes(), 2);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn a_dropped_cancel_sender_neither_cancels_nor_resends_the_request() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock.transcriber();
            transcriber.load("4bit").await.unwrap();
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let request = transcriber.transcribe(&mock.audio, None, cancel);
            let drop_sender_once_running = async {
                mock.wait_for_event("transcribe", 1).await;
                drop(cancel_tx);
                tokio::time::sleep(Duration::from_millis(200)).await;
                mock.release();
            };
            let (result, ()) = tokio::join!(request, drop_sender_once_running);
            assert_eq!(result.unwrap().text, "mock transcription");

            // A resent request would leave a stale response that fails this one.
            mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(
                mock.event_names(),
                ["load 4bit", "transcribe", "transcribe"]
            );
            assert_eq!(mock.processes(), 1);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn cancelling_a_load_stops_it_in_bounded_time() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock.transcriber();
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let load = transcriber.load_cancellable("4bit", cancel);
            let cancel_once_loading = async {
                mock.wait_for_event("load 4bit", 1).await;
                cancel_tx.send_replace(true);
                Instant::now()
            };
            let (result, cancelled_at) = tokio::join!(load, cancel_once_loading);
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            assert!(cancelled_at.elapsed() < BOUNDED);
            assert_eq!(resident(&transcriber), None);

            mock.release();
            transcriber.load("4bit").await.unwrap();
            assert_eq!(mock.processes(), 2);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn shutdown_does_not_wait_for_a_load() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = Arc::new(mock.transcriber());
            mock.hold();
            let load = tokio::spawn({
                let transcriber = transcriber.clone();
                async move { transcriber.load("4bit").await }
            });
            mock.wait_for_event("load 4bit", 1).await;

            let started = Instant::now();
            transcriber.shutdown().await.unwrap();
            assert!(started.elapsed() < BOUNDED);
            let result = load.await.unwrap();
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            mock.release();
        }

        #[tokio::test]
        async fn reconfigure_does_not_wait_for_a_load_and_switches_the_command() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = Arc::new(mock.transcriber());
            mock.hold();
            let load = tokio::spawn({
                let transcriber = transcriber.clone();
                async move { transcriber.load("4bit").await }
            });
            mock.wait_for_event("load 4bit", 1).await;

            let mut command = mock.command.clone();
            command
                .env
                .push(("ASR_WORKER_MOCK_TEXT".into(), "reconfigured".into()));
            let started = Instant::now();
            transcriber.reconfigure(command).await;
            assert!(started.elapsed() < BOUNDED);
            let result = load.await.unwrap();
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");

            mock.release();
            transcriber.load("4bit").await.unwrap();
            assert_eq!(
                mock.transcribe(&transcriber).await.unwrap().text,
                "reconfigured"
            );
            assert_eq!(mock.processes(), 2);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn reconfigure_with_the_current_command_keeps_a_running_load() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = Arc::new(mock.transcriber());
            mock.hold();
            let load = tokio::spawn({
                let transcriber = transcriber.clone();
                async move { transcriber.load("4bit").await }
            });
            mock.wait_for_event("load 4bit", 1).await;

            let started = Instant::now();
            transcriber.reconfigure(mock.command.clone()).await;
            assert!(started.elapsed() < BOUNDED);
            assert!(!load.is_finished());

            mock.release();
            load.await.unwrap().unwrap();
            assert_eq!(resident(&transcriber), Some(1));
            mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(mock.event_names(), ["load 4bit", "transcribe"]);
            assert_eq!(mock.processes(), 1);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn shutdown_kills_a_worker_whose_load_was_abandoned() {
            // Quit aborts the startup load task, dropping its future while
            // the worker is still loading; shutdown must not wait for it.
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = Arc::new(
                mock.transcriber()
                    .with_grace_periods(Duration::from_secs(5), Duration::from_secs(30)),
            );
            mock.hold();
            let load = tokio::spawn({
                let transcriber = transcriber.clone();
                async move { transcriber.load("4bit").await }
            });
            mock.wait_for_event("load 4bit", 1).await;
            load.abort();
            assert!(load.await.unwrap_err().is_cancelled());

            let started = Instant::now();
            transcriber.shutdown().await.unwrap();
            assert!(started.elapsed() < BOUNDED);
            assert!(transcriber.state.lock().await.running.is_none());
            mock.release();
        }

        #[tokio::test]
        async fn a_request_after_an_abandoned_load_uses_a_new_worker() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = Arc::new(mock.transcriber());
            mock.hold();
            let load = tokio::spawn({
                let transcriber = transcriber.clone();
                async move { transcriber.load("4bit").await }
            });
            mock.wait_for_event("load 4bit", 1).await;
            load.abort();
            let _ = load.await;
            mock.release();

            // The stale load response is never read as this load's reply.
            transcriber.load("8bit").await.unwrap();
            mock.transcribe(&transcriber).await.unwrap();
            assert_eq!(mock.processes(), 2);
            transcriber.shutdown().await.unwrap();
        }

        /// A worker from before `cancel` existed: it handles one request at a
        /// time and rejects `cancel` as unknown after the transcription.
        const LEGACY_WORKER: &str = r#"
import json, sys
from asr_worker.backends import MockBackend
from asr_worker.worker import Worker, error_response
worker = Worker(MockBackend())
for line in sys.stdin:
    request = json.loads(line)
    if request.get("command") == "cancel":
        response = error_response(request["id"], "unsupported_operation", "v1")
    else:
        response, _ = worker.handle(request)
    sys.stdout.write(json.dumps(response) + "\n")
    sys.stdout.flush()
"#;

        #[tokio::test]
        async fn a_legacy_worker_answering_cancel_last_is_drained_and_kept() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let mut command = mock.command.clone();
            command.args = vec!["-c".into(), LEGACY_WORKER.into()];
            let transcriber =
                JsonlTranscriber::new(command, Duration::from_secs(20), Duration::from_secs(20))
                    .with_grace_periods(Duration::from_secs(5), Duration::from_secs(2));
            transcriber.load("4bit").await.unwrap();
            mock.hold();

            let (cancel_tx, cancel) = watch::channel(false);
            let request = transcriber.transcribe(&mock.audio, None, cancel);
            let cancel_once_running = async {
                mock.wait_for_event("transcribe", 1).await;
                cancel_tx.send_replace(true);
            };
            let (result, ()) = tokio::join!(request, cancel_once_running);
            assert!(matches!(result, Err(AsrError::Cancelled)), "{result:?}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            mock.release();

            mock.transcribe(&transcriber).await.unwrap();
            assert!(resident(&transcriber).is_some());
            assert_eq!(
                mock.event_names(),
                ["load 4bit", "transcribe", "transcribe"]
            );
            assert_eq!(mock.processes(), 1);
            transcriber.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn shutdown_kills_a_worker_that_does_not_answer() {
            let Some(mock) = MockWorker::new(&[]) else {
                return;
            };
            let transcriber = mock
                .transcriber()
                .with_grace_periods(Duration::from_secs(5), Duration::from_millis(500));
            transcriber.load("4bit").await.unwrap();
            // The mock's unload, which `shutdown` runs, now never returns.
            mock.hold();

            let started = Instant::now();
            let result = transcriber.shutdown().await;
            assert!(matches!(result, Err(AsrError::Timeout)), "{result:?}");
            assert!(started.elapsed() < BOUNDED);
            assert!(transcriber.state.lock().await.running.is_none());
        }
    }
}

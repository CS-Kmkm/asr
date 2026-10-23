//! Size-capped persistence of the ASR worker's stderr.
//!
//! The worker reports `internal_error` details only on stderr. A release build
//! has no console to inherit, so the stream is piped into a log file that is
//! rotated once it would exceed [`MAX_LOG_BYTES`], keeping one previous file.
//! Only the worker's own diagnostics, content-free process markers, and the
//! app's startup paths (worker project root, `.env` location, and database)
//! are written here; the Rust side never adds transcript, audio, window-title,
//! clipboard, or environment-variable content.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use tokio::io::{AsyncRead, AsyncReadExt};

/// Largest size of the active log file before it is rotated.
pub(crate) const MAX_LOG_BYTES: u64 = 1024 * 1024;
/// File name of the active worker log inside the app log directory.
pub(crate) const LOG_FILE_NAME: &str = "asr-worker.log";

static SINK: OnceLock<Mutex<RotatingLog>> = OnceLock::new();

/// Append-only log file with a single-generation rotation.
///
/// When a write would push the active file past `max_bytes`, the active file
/// replaces `<name>.1` (discarding the older generation) and a fresh file is
/// started, so disk use stays below roughly twice the limit. If another
/// process blocks the rename (an open handle without delete sharing), the
/// active file is truncated instead so logging continues.
pub(crate) struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    file: Option<File>,
    written: u64,
    /// Active-file size that triggers the next rotation attempt. Raised after
    /// an attempt that could neither rename nor truncate, so a blocked file is
    /// retried once per further `max_bytes` rather than on every write.
    rotate_at: u64,
}

impl RotatingLog {
    pub(crate) fn new(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes: max_bytes.max(1),
            file: None,
            written: 0,
            rotate_at: max_bytes.max(1),
        }
    }

    pub(crate) fn previous_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        // A single chunk larger than the cap keeps only its newest tail.
        let cap = usize::try_from(self.max_bytes).unwrap_or(usize::MAX);
        let bytes = &bytes[bytes.len().saturating_sub(cap)..];
        self.ensure_open()?;
        if self.written > 0 && self.written + bytes.len() as u64 > self.rotate_at {
            self.rotate();
        }
        let file = self.ensure_open()?;
        file.write_all(bytes)?;
        file.flush()?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    fn ensure_open(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            self.written = file.metadata()?.len();
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("log file was just opened"))
    }

    fn rotate(&mut self) {
        // Windows cannot rename an open file or onto an existing one.
        self.file = None;
        let reset = self
            .replace_previous()
            .or_else(|_| self.truncate_active())
            .is_ok();
        if reset {
            self.written = 0;
            self.rotate_at = self.max_bytes;
        } else {
            // Keep appending to the oversized file; the next write reopens it.
            self.rotate_at = self.written.saturating_add(self.max_bytes);
        }
    }

    fn replace_previous(&self) -> io::Result<()> {
        let previous = self.previous_path();
        match fs::remove_file(&previous) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&self.path, &previous)
    }

    fn truncate_active(&self) -> io::Result<()> {
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)
            .map(drop)
    }
}

/// Route future worker stderr output into `directory/asr-worker.log`.
///
/// Only the first call takes effect. Without a sink, debug builds (unit tests,
/// benchmarks) keep inheriting the parent's stderr and release builds discard
/// it.
pub(crate) fn install(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let _ = SINK.set(Mutex::new(RotatingLog::new(
        directory.join(LOG_FILE_NAME),
        MAX_LOG_BYTES,
    )));
    Ok(())
}

pub(crate) fn is_installed() -> bool {
    SINK.get().is_some()
}

fn append(bytes: &[u8]) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let mut log = sink.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // Logging is best effort and must never disturb transcription.
    let _ = log.write(bytes);
}

fn timestamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Record an app start with its content-free startup diagnostics, so the
/// worker discovery is visible in release builds that have no console.
pub(crate) fn record_app_start(diagnostics: &str) {
    let record = format!("--- app started {} ---\n{diagnostics}", timestamp());
    append(record.as_bytes());
}

/// Append a content-free startup detail (such as the database path) that is
/// known only after the app start was recorded.
pub(crate) fn record_startup_detail(detail: &str) {
    append(detail.as_bytes());
}

/// Record that a new worker process started, without any content.
pub(crate) fn mark_worker_start() {
    let marker = format!("--- ASR worker started {} ---\n", timestamp());
    append(marker.as_bytes());
}

/// Third-party libraries may write paths, request bodies or keys to stderr.
/// Persist only structured, allowlisted diagnostics and fixed native hints.
fn filtered_worker_line(line: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(line).ok()?.trim();
    let raw = match text.strip_prefix("ASR_DIAGNOSTIC ") {
        Some(raw) => raw,
        None => {
            if text.to_ascii_lowercase().contains("cublas64_12.dll") {
                return Some(
                    b"worker diagnostic: cuBLAS 12 library could not be loaded\n".to_vec(),
                );
            }
            return None;
        }
    };
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let operation = value.get("operation")?.as_str()?;
    if value.get("event")?.as_str()? != "exception"
        || !matches!(operation, "load" | "transcribe" | "startup" | "internal")
    {
        return None;
    }
    let chain = value.get("chain")?.as_array()?;
    if chain.len() > 8 {
        return None;
    }
    let mut safe_chain = Vec::new();
    for cause in chain {
        let kind = cause.get("type")?.as_str()?;
        let kind = if matches!(
            kind,
            "RuntimeError"
                | "ValueError"
                | "TypeError"
                | "OSError"
                | "FileNotFoundError"
                | "PermissionError"
                | "ModuleNotFoundError"
                | "ImportError"
                | "MemoryError"
                | "BackendError"
                | "HTTPError"
                | "ReadTimeout"
                | "ConnectTimeout"
                | "Exception"
        ) {
            kind
        } else {
            "Exception"
        };
        let hint = cause.get("hint")?.as_str()?;
        let hint = if matches!(
            hint,
            "missing_cublas12"
                | "missing_cudnn9"
                | "out_of_memory"
                | "tokenizer_configuration"
                | "model_file"
                | "invalid_device"
                | "invalid_api_config"
                | "unsupported_backend"
                | "serve_auth_required"
                | "serve_dependencies_missing"
        ) {
            hint
        } else {
            "details_redacted"
        };
        let frames = cause.get("frames")?.as_array()?;
        if frames.len() > 24 {
            return None;
        }
        let safe_frames: Vec<_> = frames
            .iter()
            .filter_map(|frame| {
                let module = frame.get("module")?.as_str()?;
                let module = if matches!(
                    module,
                    "asr_worker.backends"
                        | "asr_worker.worker"
                        | "asr_worker.api"
                        | "asr_worker.download"
                ) {
                    module
                } else {
                    "dependency"
                };
                let line = frame.get("line")?.as_u64()?.min(100_000);
                Some(serde_json::json!({"module": module, "line": line}))
            })
            .collect();
        safe_chain.push(serde_json::json!({"type": kind, "hint": hint, "frames": safe_frames}));
    }
    Some(
        format!(
            "ASR_DIAGNOSTIC {}\n",
            serde_json::json!({"event": "exception", "operation": operation, "chain": safe_chain})
        )
        .into_bytes(),
    )
}

/// Copy safe worker diagnostics, bounding an unterminated line as well as disk.
pub(crate) async fn pump<R: AsyncRead + Unpin>(mut stderr: R) {
    let mut buffer = [0_u8; 8 * 1024];
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                for byte in &buffer[..read] {
                    if *byte == b'\n' {
                        if !oversized {
                            if let Some(safe) = filtered_worker_line(&line) {
                                append(&safe);
                            }
                        }
                        line.clear();
                        oversized = false;
                    } else if !oversized {
                        if line.len() < 64 * 1024 {
                            line.push(*byte);
                        } else {
                            line.clear();
                            oversized = true;
                        }
                    }
                }
            }
        }
    }
    if !oversized {
        if let Some(safe) = filtered_worker_line(&line) {
            append(&safe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_diagnostics_keep_stack_locations_without_arbitrary_text() {
        let raw = br#"ASR_DIAGNOSTIC {"event":"exception","operation":"transcribe","secret":"private transcript","chain":[{"type":"RuntimeError","hint":"missing_cublas12","message":"hf_secret C:/private/audio.wav private transcript","frames":[{"module":"asr_worker.backends","function":"private transcript","line":123}]}]}"#;
        let filtered = String::from_utf8(filtered_worker_line(raw).unwrap()).unwrap();
        assert!(filtered.contains("asr_worker.backends"));
        assert!(filtered.contains("123"));
        assert!(filtered.contains("missing_cublas12"));
        for secret in ["private", "hf_secret", "audio.wav", "C:/"] {
            assert!(!filtered.contains(secret));
        }
        assert!(filtered_worker_line(
            b"unstructured library message containing private transcript"
        )
        .is_none());
        let hint = filtered_worker_line(b"C:/private/key CUDA failed cublas64_12.dll").unwrap();
        assert!(!String::from_utf8(hint).unwrap().contains("private"));
    }

    fn read(path: &Path) -> Vec<u8> {
        fs::read(path).unwrap_or_default()
    }

    #[test]
    fn appends_below_the_limit_without_rotating() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 10);
        log.write(b"abc").unwrap();
        log.write(b"defg").unwrap();
        assert_eq!(read(&path), b"abcdefg");
        assert!(!log.previous_path().exists());
    }

    #[test]
    fn rotates_when_a_write_would_exceed_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 10);
        log.write(b"0123456").unwrap();
        log.write(b"7890").unwrap();
        assert_eq!(read(&log.previous_path()), b"0123456");
        assert_eq!(read(&path), b"7890");
    }

    #[test]
    fn keeps_only_one_previous_generation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 4);
        log.write(b"aaaa").unwrap();
        log.write(b"bbbb").unwrap();
        log.write(b"cccc").unwrap();
        assert_eq!(read(&log.previous_path()), b"bbbb");
        assert_eq!(read(&path), b"cccc");
        let files = fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(files, 2);
    }

    #[test]
    fn a_write_filling_the_limit_exactly_does_not_rotate() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 6);
        log.write(b"abc").unwrap();
        log.write(b"def").unwrap();
        assert_eq!(read(&path), b"abcdef");
        assert!(!log.previous_path().exists());
    }

    #[test]
    fn an_oversized_chunk_keeps_its_newest_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 4);
        log.write(b"0123456789").unwrap();
        assert_eq!(read(&path), b"6789");
    }

    #[test]
    fn a_blocked_rename_falls_back_to_truncating_the_active_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        let mut log = RotatingLog::new(path.clone(), 4);
        // A non-empty directory at the previous path can be neither removed
        // as a file nor replaced by a rename, like a file another process
        // holds open without delete sharing.
        let previous = log.previous_path();
        fs::create_dir(&previous).unwrap();
        fs::write(previous.join("held"), b"x").unwrap();
        log.write(b"aaaa").unwrap();
        log.write(b"bbbb").unwrap();
        assert_eq!(read(&path), b"bbbb");
        log.write(b"cc").unwrap();
        // Each later rotation still succeeds by truncating, never dropping.
        assert_eq!(read(&path), b"cc");
        assert!(previous.is_dir());
    }

    #[test]
    fn resumes_the_size_of_an_existing_log_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(LOG_FILE_NAME);
        fs::write(&path, b"earlier").unwrap();
        let mut log = RotatingLog::new(path.clone(), 10);
        log.write(b"later").unwrap();
        assert_eq!(read(&log.previous_path()), b"earlier");
        assert_eq!(read(&path), b"later");
    }
}

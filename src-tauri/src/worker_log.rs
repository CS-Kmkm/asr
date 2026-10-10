//! Size-capped persistence of the ASR worker's stderr.
//!
//! The worker reports `internal_error` details only on stderr. A release build
//! has no console to inherit, so the stream is piped into a log file that is
//! rotated once it would exceed [`MAX_LOG_BYTES`], keeping one previous file.
//! Only the worker's own diagnostics and content-free process markers are
//! written here; the Rust side never adds transcript, audio, window-title, or
//! clipboard content.

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

/// Record that a new worker process started, without any content.
pub(crate) fn mark_worker_start() {
    let marker = format!(
        "--- ASR worker started {} ---\n",
        chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    );
    append(marker.as_bytes());
}

/// Copy a worker's stderr into the installed sink until the stream closes.
pub(crate) async fn pump<R: AsyncRead + Unpin>(mut stderr: R) {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => append(&buffer[..read]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

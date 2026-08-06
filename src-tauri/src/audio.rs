//! Microphone capture, deterministic audio preprocessing, and WAV artifact creation.
//!
//! The CPAL backend is intentionally Windows-only. This lets non-Windows CI exercise
//! the DSP and lifecycle code without requiring native ALSA development packages.

use serde::Serialize;
use std::fmt;
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
#[cfg(target_os = "windows")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(target_os = "windows")]
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const TARGET_SAMPLE_RATE: u32 = 24_000;

/// Default amount of audio retained ahead of a hotkey press so the leading edge
/// of speech is never clipped by stream warm-up latency.
pub const DEFAULT_PREROLL: Duration = Duration::from_millis(300);

pub type AudioFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AudioError>> + Send + 'a>>;

/// Capture lifecycle contract used by commands and mock implementations.
pub trait AudioCapture: Send {
    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>>;
    /// Pre-arm the input stream so samples flow into the preroll ring before a
    /// recording starts. Idempotent: a no-op when already armed or capturing.
    fn arm(&mut self, config: CaptureConfig) -> AudioFuture<'_, ()>;
    /// Release a previously armed stream. Idempotent and a no-op while capturing.
    fn disarm(&mut self) -> AudioFuture<'_, ()>;
    fn start(&mut self, config: CaptureConfig) -> AudioFuture<'_, ()>;
    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact>;
    fn cancel(&mut self) -> AudioFuture<'_, ()>;
    fn state(&self) -> CaptureState;
    fn level(&self) -> LevelMeter;
}

/// Boundary for replacing the deterministic fallback with WebRTC VAD later.
pub trait VoiceActivityDetector: Send + Sync {
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> VadAnalysis;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    /// Stable only for the lifetime of the current device enumeration.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub device_id: Option<String>,
    pub target_sample_rate: u32,
    pub minimum_duration: Duration,
    /// Amount of pre-press audio retained in the preroll ring. `Duration::ZERO`
    /// disables prerolling and matches the legacy "open on press" behaviour.
    pub preroll: Duration,
    pub vad: EnergyVadConfig,
    pub artifact_directory: Option<PathBuf>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            target_sample_rate: TARGET_SAMPLE_RATE,
            minimum_duration: Duration::from_millis(250),
            preroll: DEFAULT_PREROLL,
            vad: EnergyVadConfig::default(),
            artifact_directory: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioArtifact {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_count: usize,
    pub duration: Duration,
    pub peak_level: f32,
    pub rms_level: f32,
    pub vad: VadAnalysis,
}

impl AudioArtifact {
    /// Explicit cleanup helper; artifacts are not deleted automatically on drop.
    pub fn remove(self) -> Result<(), AudioError> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AudioError::Io(error)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LevelMeter {
    /// Root mean square amplitude in the normalized 0.0..=1.0 range.
    pub rms: f32,
    /// Peak absolute amplitude in the normalized 0.0..=1.0 range.
    pub peak: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureState {
    Idle,
    Capturing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnergyVadConfig {
    pub frame_duration: Duration,
    pub rms_threshold: f32,
    pub minimum_voice_duration: Duration,
}

impl Default for EnergyVadConfig {
    fn default() -> Self {
        Self {
            frame_duration: Duration::from_millis(20),
            rms_threshold: 0.01,
            minimum_voice_duration: Duration::from_millis(100),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VadAnalysis {
    pub contains_voice: bool,
    pub voiced_duration: Duration,
    pub trailing_silence: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct EnergyVad {
    config: EnergyVadConfig,
}

impl EnergyVad {
    pub fn new(config: EnergyVadConfig) -> Self {
        Self { config }
    }
}

impl VoiceActivityDetector for EnergyVad {
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> VadAnalysis {
        if samples.is_empty() || sample_rate == 0 {
            return VadAnalysis::default();
        }

        let frame_len = duration_to_samples(self.config.frame_duration, sample_rate).max(1);
        let mut voiced_samples = 0usize;
        let mut trailing_silent_samples = 0usize;
        let mut seen_voice = false;

        for frame in samples.chunks(frame_len) {
            if meter(frame).rms >= self.config.rms_threshold {
                voiced_samples += frame.len();
                trailing_silent_samples = 0;
                seen_voice = true;
            } else if seen_voice {
                trailing_silent_samples += frame.len();
            }
        }

        let voiced_duration = samples_to_duration(voiced_samples, sample_rate);
        VadAnalysis {
            contains_voice: voiced_duration >= self.config.minimum_voice_duration,
            voiced_duration,
            trailing_silence: samples_to_duration(trailing_silent_samples, sample_rate),
        }
    }
}

/// Fixed-capacity ring of the most recent mono samples.
///
/// The buffer retains at most `capacity` samples (derived from a preroll
/// duration and sample rate). Pushing past the capacity discards the oldest
/// samples so the buffer always reflects the latest window of audio. This lets
/// `start()` prepend the audio that arrived just before a hotkey press, so the
/// leading edge of speech is never clipped by stream warm-up latency.
///
/// The ring stores samples in arrival order and `drain()` returns them oldest
/// first, ready to be prepended to the recording buffer.
#[derive(Clone, Debug, Default)]
pub struct PrerollBuffer {
    buffer: Vec<f32>,
    capacity: usize,
    /// Index of the oldest sample; only meaningful once the ring is full.
    head: usize,
    full: bool,
}

impl PrerollBuffer {
    /// Creates a ring sized to hold `preroll` worth of audio at `sample_rate`.
    /// A zero duration or sample rate yields a zero-capacity (disabled) ring.
    pub fn new(preroll: Duration, sample_rate: u32) -> Self {
        Self::with_capacity(duration_to_samples(preroll, sample_rate))
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(capacity),
            capacity,
            head: 0,
            full: false,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of samples currently retained, never exceeding `capacity`.
    pub fn len(&self) -> usize {
        if self.full {
            self.capacity
        } else {
            self.buffer.len()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `samples`, evicting the oldest entries once capacity is reached.
    pub fn extend(&mut self, samples: &[f32]) {
        if self.capacity == 0 {
            return;
        }
        for &sample in samples {
            self.push(sample);
        }
    }

    fn push(&mut self, sample: f32) {
        if self.capacity == 0 {
            return;
        }
        if self.full {
            self.buffer[self.head] = sample;
            self.head = (self.head + 1) % self.capacity;
        } else {
            self.buffer.push(sample);
            if self.buffer.len() == self.capacity {
                self.full = true;
                self.head = 0;
            }
        }
    }

    /// Returns the retained samples in oldest-first order without clearing them.
    pub fn snapshot(&self) -> Vec<f32> {
        if !self.full {
            return self.buffer.clone();
        }
        let mut out = Vec::with_capacity(self.capacity);
        out.extend_from_slice(&self.buffer[self.head..]);
        out.extend_from_slice(&self.buffer[..self.head]);
        out
    }

    /// Returns the retained samples (oldest first) and resets the ring.
    pub fn drain(&mut self) -> Vec<f32> {
        let out = self.snapshot();
        self.clear();
        out
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.head = 0;
        self.full = false;
    }
}

#[derive(Debug)]
pub enum AudioError {
    AlreadyCapturing,
    NotCapturing,
    Cancelled,
    UnsupportedPlatform,
    DeviceNotFound(String),
    DeviceEnumeration(String),
    DeviceConfiguration(String),
    StreamBuild(String),
    StreamPlay(String),
    StreamFailure(String),
    InvalidConfiguration(&'static str),
    TooShort { actual: Duration, minimum: Duration },
    NoVoiceDetected,
    Io(io::Error),
}

impl fmt::Display for AudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyCapturing => write!(formatter, "audio capture is already running"),
            Self::NotCapturing => write!(formatter, "audio capture is not running"),
            Self::Cancelled => write!(formatter, "audio capture was cancelled"),
            Self::UnsupportedPlatform => {
                write!(formatter, "microphone capture is supported on Windows only")
            }
            Self::DeviceNotFound(id) => write!(formatter, "audio input device not found: {id}"),
            Self::DeviceEnumeration(message) => {
                write!(formatter, "failed to enumerate audio devices: {message}")
            }
            Self::DeviceConfiguration(message) => {
                write!(formatter, "failed to configure audio device: {message}")
            }
            Self::StreamBuild(message) => {
                write!(formatter, "failed to build input stream: {message}")
            }
            Self::StreamPlay(message) => {
                write!(formatter, "failed to start input stream: {message}")
            }
            Self::StreamFailure(message) => {
                write!(formatter, "audio input stream failed: {message}")
            }
            Self::InvalidConfiguration(message) => {
                write!(formatter, "invalid audio configuration: {message}")
            }
            Self::TooShort { actual, minimum } => write!(
                formatter,
                "recording is too short ({} ms, minimum {} ms)",
                actual.as_millis(),
                minimum.as_millis()
            ),
            Self::NoVoiceDetected => write!(formatter, "recording contains no detected voice"),
            Self::Io(error) => write!(formatter, "audio artifact I/O failed: {error}"),
        }
    }
}

impl std::error::Error for AudioError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for AudioError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Default)]
struct SharedCapture {
    /// Samples accumulated while `recording` is true.
    samples: Vec<f32>,
    /// Most recent pre-press audio; drained into `samples` when recording starts.
    preroll: PrerollBuffer,
    /// Whether the stream callback is currently accumulating into `samples`.
    recording: bool,
    level: LevelMeter,
    stream_error: Option<String>,
}

impl SharedCapture {
    fn armed(preroll: PrerollBuffer) -> Self {
        Self {
            preroll,
            ..Self::default()
        }
    }

    /// Transitions the callback into recording mode, seeding `samples` with the
    /// retained preroll so the leading edge of speech is preserved.
    fn begin_recording(&mut self) {
        if self.recording {
            return;
        }
        self.samples = self.preroll.drain();
        self.recording = true;
    }
}

/// An open input stream that is either armed (preroll only) or recording.
struct ActiveCapture {
    config: CaptureConfig,
    input_sample_rate: u32,
    shared: Arc<Mutex<SharedCapture>>,
    #[cfg(target_os = "windows")]
    stream_worker: StreamWorker,
}

/// Owns the CPAL stream thread without moving `cpal::Stream` between threads.
///
/// CPAL's platform-erased stream is deliberately `!Send`, so the stream must
/// remain on the thread where it was created. This handle itself is `Send` and
/// can safely live in Tauri's managed audio service.
#[cfg(target_os = "windows")]
struct StreamWorker {
    shutdown: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}

#[cfg(target_os = "windows")]
impl StreamWorker {
    fn shutdown(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for StreamWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// CPAL microphone implementation on Windows and an API-compatible stub elsewhere.
#[derive(Default)]
pub struct CpalAudioCapture {
    active: Option<ActiveCapture>,
    /// True once `start()` has promoted the active stream to recording mode.
    recording: bool,
}

impl CpalAudioCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the active stream is recording (as opposed to merely armed).
    fn is_recording(&self) -> bool {
        self.recording && self.active.is_some()
    }

    fn finish(&mut self, cancelled: bool) -> Result<AudioArtifact, AudioError> {
        if !self.is_recording() {
            return Err(AudioError::NotCapturing);
        }
        let mut active = self.active.take().ok_or(AudioError::NotCapturing)?;
        self.recording = false;
        drop_active_stream(&mut active);

        if cancelled {
            return Err(AudioError::Cancelled);
        }

        let mut shared = active
            .shared
            .lock()
            .map_err(|_| AudioError::StreamFailure("capture buffer lock poisoned".into()))?;
        if let Some(message) = &shared.stream_error {
            return Err(AudioError::StreamFailure(message.clone()));
        }

        shared.recording = false;
        let mono = std::mem::take(&mut shared.samples);
        drop(shared);
        finalize_samples(mono, active.input_sample_rate, &active.config)
    }
}

impl AudioCapture for CpalAudioCapture {
    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
        Box::pin(async move { list_cpal_devices() })
    }

    fn arm(&mut self, config: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async move {
            // Idempotent: already armed or recording leaves the stream untouched.
            if self.active.is_some() {
                return Ok(());
            }
            validate_config(&config)?;
            self.active = Some(start_cpal_capture(config)?);
            self.recording = false;
            Ok(())
        })
    }

    fn disarm(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async move {
            // Never tear down a stream that is actively recording.
            if self.is_recording() {
                return Ok(());
            }
            if let Some(mut active) = self.active.take() {
                drop_active_stream(&mut active);
            }
            Ok(())
        })
    }

    fn start(&mut self, config: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async move {
            if self.is_recording() {
                return Err(AudioError::AlreadyCapturing);
            }
            validate_config(&config)?;
            // Reuse the armed stream (preserving its preroll) when present, so the
            // leading edge of speech captured during warm-up is not lost.
            let active = match self.active.take() {
                Some(active) => active,
                None => start_cpal_capture(config)?,
            };
            {
                let mut shared = active.shared.lock().map_err(|_| {
                    AudioError::StreamFailure("capture buffer lock poisoned".into())
                })?;
                if let Some(message) = &shared.stream_error {
                    return Err(AudioError::StreamFailure(message.clone()));
                }
                shared.begin_recording();
            }
            self.active = Some(active);
            self.recording = true;
            Ok(())
        })
    }

    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
        Box::pin(async move { self.finish(false) })
    }

    fn cancel(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async move {
            match self.finish(true) {
                Err(AudioError::Cancelled) => Ok(()),
                Err(error) => Err(error),
                Ok(_) => Ok(()),
            }
        })
    }

    fn state(&self) -> CaptureState {
        if self.is_recording() {
            CaptureState::Capturing
        } else {
            CaptureState::Idle
        }
    }

    fn level(&self) -> LevelMeter {
        self.active
            .as_ref()
            .and_then(|active| active.shared.lock().ok().map(|shared| shared.level))
            .unwrap_or_default()
    }
}

fn validate_config(config: &CaptureConfig) -> Result<(), AudioError> {
    if config.target_sample_rate == 0 {
        return Err(AudioError::InvalidConfiguration(
            "target sample rate must be greater than zero",
        ));
    }
    if !config.vad.rms_threshold.is_finite() || config.vad.rms_threshold < 0.0 {
        return Err(AudioError::InvalidConfiguration(
            "VAD RMS threshold must be finite and non-negative",
        ));
    }
    Ok(())
}

fn finalize_samples(
    mono_samples: Vec<f32>,
    input_sample_rate: u32,
    config: &CaptureConfig,
) -> Result<AudioArtifact, AudioError> {
    let samples = resample_linear(&mono_samples, input_sample_rate, config.target_sample_rate);
    let duration = samples_to_duration(samples.len(), config.target_sample_rate);
    if duration < config.minimum_duration {
        return Err(AudioError::TooShort {
            actual: duration,
            minimum: config.minimum_duration,
        });
    }

    let vad = EnergyVad::new(config.vad).analyze(&samples, config.target_sample_rate);
    if !vad.contains_voice {
        return Err(AudioError::NoVoiceDetected);
    }

    let levels = meter(&samples);
    let path = temporary_wav_path(config.artifact_directory.as_deref());
    if let Err(error) = write_pcm16_wav(&path, &samples, config.target_sample_rate) {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(AudioArtifact {
        path,
        sample_rate: config.target_sample_rate,
        channels: 1,
        sample_count: samples.len(),
        duration,
        peak_level: levels.peak,
        rms_level: levels.rms,
        vad,
    })
}

/// Averages complete interleaved frames into normalized mono samples.
pub fn interleaved_to_mono(samples: &[f32], channels: u16) -> Vec<f32> {
    let channels = channels as usize;
    if channels == 0 {
        return Vec::new();
    }
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().copied().sum::<f32>() / channels as f32)
        .collect()
}

/// Deterministic linear interpolation resampler suitable for ASR preprocessing.
pub fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if samples.is_empty() || source_rate == 0 || target_rate == 0 {
        return Vec::new();
    }
    if source_rate == target_rate {
        return samples.to_vec();
    }

    let output_len = ((samples.len() as u64 * target_rate as u64) / source_rate as u64) as usize;
    if output_len == 0 {
        return Vec::new();
    }

    let scale = source_rate as f64 / target_rate as f64;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * scale;
            let left = position.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (position - left as f64) as f32;
            samples[left] + (samples[right] - samples[left]) * fraction
        })
        .collect()
}

pub fn meter(samples: &[f32]) -> LevelMeter {
    if samples.is_empty() {
        return LevelMeter::default();
    }
    let mut peak = 0.0f32;
    let mut sum_squares = 0.0f64;
    for sample in samples {
        let value = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        peak = peak.max(value.abs());
        sum_squares += f64::from(value) * f64::from(value);
    }
    LevelMeter {
        rms: (sum_squares / samples.len() as f64).sqrt() as f32,
        peak,
    }
}

fn write_pcm16_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), AudioError> {
    let data_size = samples
        .len()
        .checked_mul(2)
        .and_then(|size| u32::try_from(size).ok())
        .ok_or(AudioError::InvalidConfiguration(
            "WAV artifact is too large",
        ))?;
    let riff_size = 36u32
        .checked_add(data_size)
        .ok_or(AudioError::InvalidConfiguration(
            "WAV artifact is too large",
        ))?;
    let mut writer = BufWriter::new(File::create(path)?);

    writer.write_all(b"RIFF")?;
    writer.write_all(&riff_size.to_le_bytes())?;
    writer.write_all(b"WAVEfmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&(sample_rate * 2).to_le_bytes())?;
    writer.write_all(&2u16.to_le_bytes())?;
    writer.write_all(&16u16.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_size.to_le_bytes())?;
    for sample in samples {
        let normalized = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        let value = (normalized * i16::MAX as f32).round() as i16;
        writer.write_all(&value.to_le_bytes())?;
    }
    writer.flush()?;
    Ok(())
}

fn temporary_wav_path(directory: Option<&Path>) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    directory
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!(
            "local-ai-voice-{}-{timestamp}-{nonce}.wav",
            std::process::id()
        ))
}

fn duration_to_samples(duration: Duration, sample_rate: u32) -> usize {
    ((duration.as_nanos() * u128::from(sample_rate)) / 1_000_000_000) as usize
}

fn samples_to_duration(sample_count: usize, sample_rate: u32) -> Duration {
    if sample_rate == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(sample_count as f64 / sample_rate as f64)
    }
}

#[cfg(target_os = "windows")]
fn list_cpal_devices() -> Result<Vec<AudioDevice>, AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait};

    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|device| device.name().ok());
    let devices = host
        .input_devices()
        .map_err(|error| AudioError::DeviceEnumeration(error.to_string()))?;

    devices
        .enumerate()
        .map(|(index, device)| {
            let name = device
                .name()
                .map_err(|error| AudioError::DeviceEnumeration(error.to_string()))?;
            Ok(AudioDevice {
                id: format!("{index}:{name}"),
                is_default: default_name.as_deref() == Some(name.as_str()),
                name,
            })
        })
        .collect()
}

#[cfg(not(target_os = "windows"))]
fn list_cpal_devices() -> Result<Vec<AudioDevice>, AudioError> {
    Err(AudioError::UnsupportedPlatform)
}

#[cfg(target_os = "windows")]
fn start_cpal_capture(config: CaptureConfig) -> Result<ActiveCapture, AudioError> {
    let (startup_tx, startup_rx) = mpsc::sync_channel(1);
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let active_config = config.clone();
    let thread = thread::Builder::new()
        .name("cpal-input-stream".into())
        .spawn(move || {
            let startup = start_cpal_stream(config);
            match startup {
                Ok((input_sample_rate, shared, stream)) => {
                    if startup_tx.send(Ok((input_sample_rate, shared))).is_err() {
                        return;
                    }
                    let _ = shutdown_rx.recv();
                    use cpal::traits::StreamTrait;
                    let _ = stream.pause();
                }
                Err(error) => {
                    let _ = startup_tx.send(Err(error));
                }
            }
        })
        .map_err(|error| AudioError::StreamBuild(error.to_string()))?;

    let (input_sample_rate, shared) = match startup_rx.recv() {
        Ok(Ok(startup)) => startup,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(error) => {
            let _ = thread.join();
            return Err(AudioError::StreamBuild(format!(
                "audio stream thread exited during startup: {error}"
            )));
        }
    };

    Ok(ActiveCapture {
        config: active_config,
        input_sample_rate,
        shared,
        stream_worker: StreamWorker {
            shutdown: shutdown_tx,
            thread: Some(thread),
        },
    })
}

#[cfg(target_os = "windows")]
fn start_cpal_stream(
    config: CaptureConfig,
) -> Result<(u32, Arc<Mutex<SharedCapture>>, cpal::Stream), AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = if let Some(requested_id) = config.device_id.as_deref() {
        host.input_devices()
            .map_err(|error| AudioError::DeviceEnumeration(error.to_string()))?
            .enumerate()
            .find_map(|(index, device)| {
                let name = device.name().ok()?;
                (format!("{index}:{name}") == requested_id).then_some(device)
            })
            .ok_or_else(|| AudioError::DeviceNotFound(requested_id.to_owned()))?
    } else {
        host.default_input_device()
            .ok_or_else(|| AudioError::DeviceNotFound("default".into()))?
    };
    let supported = device
        .default_input_config()
        .map_err(|error| AudioError::DeviceConfiguration(error.to_string()))?;
    let input_sample_rate = supported.sample_rate().0;
    let channels = supported.channels();
    let stream_config: cpal::StreamConfig = supported.clone().into();
    // Size the preroll ring against the device's native rate; resampling to the
    // target rate happens later in finalize_samples().
    let preroll = PrerollBuffer::new(config.preroll, input_sample_rate);
    let shared = Arc::new(Mutex::new(SharedCapture::armed(preroll)));
    let error_shared = Arc::clone(&shared);
    let error_callback = move |error: cpal::StreamError| {
        if let Ok(mut state) = error_shared.lock() {
            state.stream_error = Some(error.to_string());
        }
    };

    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_input_stream::<f32>(
            &device,
            &stream_config,
            channels,
            Arc::clone(&shared),
            error_callback,
        ),
        cpal::SampleFormat::I16 => build_input_stream::<i16>(
            &device,
            &stream_config,
            channels,
            Arc::clone(&shared),
            error_callback,
        ),
        cpal::SampleFormat::U16 => build_input_stream::<u16>(
            &device,
            &stream_config,
            channels,
            Arc::clone(&shared),
            error_callback,
        ),
        format => Err(AudioError::DeviceConfiguration(format!(
            "unsupported sample format: {format:?}"
        ))),
    }?;
    stream
        .play()
        .map_err(|error| AudioError::StreamPlay(error.to_string()))?;

    Ok((input_sample_rate, shared, stream))
}

#[cfg(not(target_os = "windows"))]
fn start_cpal_capture(_config: CaptureConfig) -> Result<ActiveCapture, AudioError> {
    Err(AudioError::UnsupportedPlatform)
}

#[cfg(target_os = "windows")]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: u16,
    shared: Arc<Mutex<SharedCapture>>,
    error_callback: impl FnMut(cpal::StreamError) + Send + 'static,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + cpal::Sample,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;

    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                let interleaved: Vec<f32> =
                    data.iter().copied().map(cpal::Sample::to_sample).collect();
                let mono = interleaved_to_mono(&interleaved, channels);
                let current_level = meter(&mono);
                if let Ok(mut state) = shared.lock() {
                    state.level = current_level;
                    if state.recording {
                        // Already recording: append directly to the take. The
                        // ring stays empty so a later start cannot double-count.
                        state.samples.extend(mono);
                    } else {
                        // Armed: retain only the latest preroll window.
                        state.preroll.extend(&mono);
                    }
                }
            },
            error_callback,
            None,
        )
        .map_err(|error| AudioError::StreamBuild(error.to_string()))
}

fn drop_active_stream(active: &mut ActiveCapture) {
    #[cfg(target_os = "windows")]
    {
        active.stream_worker.shutdown();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_interleaved_stereo_to_mono() {
        let mono = interleaved_to_mono(&[1.0, -1.0, 0.5, 0.25], 2);
        assert_eq!(mono, vec![0.0, 0.375]);
    }

    #[test]
    fn resampling_preserves_duration_and_interpolates() {
        let source = vec![0.0, 1.0, 0.0, -1.0];
        let upsampled = resample_linear(&source, 4, 8);
        assert_eq!(upsampled.len(), 8);
        assert!((upsampled[1] - 0.5).abs() < 1e-6);
        assert_eq!(resample_linear(&source, 4, 2).len(), 2);
    }

    #[test]
    fn rejects_short_audio_before_writing_artifact() {
        let config = CaptureConfig {
            minimum_duration: Duration::from_millis(100),
            ..CaptureConfig::default()
        };
        let error = finalize_samples(vec![0.5; 2_399], TARGET_SAMPLE_RATE, &config)
            .expect_err("short input must be rejected");
        assert!(matches!(error, AudioError::TooShort { .. }));
    }

    #[test]
    fn rejects_silent_audio() {
        let config = CaptureConfig {
            minimum_duration: Duration::from_millis(10),
            ..CaptureConfig::default()
        };
        let error = finalize_samples(vec![0.0; 4_800], TARGET_SAMPLE_RATE, &config)
            .expect_err("silence must be rejected");
        assert!(matches!(error, AudioError::NoVoiceDetected));
    }

    #[test]
    fn energy_vad_reports_voice_and_trailing_silence() {
        let mut samples = vec![0.1; 2_400];
        samples.extend(vec![0.0; 1_200]);
        let vad = EnergyVad::new(EnergyVadConfig {
            minimum_voice_duration: Duration::from_millis(50),
            ..EnergyVadConfig::default()
        })
        .analyze(&samples, TARGET_SAMPLE_RATE);
        assert!(vad.contains_voice);
        assert!(vad.trailing_silence >= Duration::from_millis(40));
    }

    #[test]
    fn state_transitions_reject_invalid_stop_and_cancel() {
        let mut capture = CpalAudioCapture::new();
        assert_eq!(capture.state(), CaptureState::Idle);
        assert!(matches!(
            capture.finish(false),
            Err(AudioError::NotCapturing)
        ));
        assert!(matches!(
            capture.finish(true),
            Err(AudioError::NotCapturing)
        ));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn duplicate_start_state_is_represented_without_hardware() {
        let shared = Arc::new(Mutex::new(SharedCapture::default()));
        let mut capture = CpalAudioCapture {
            active: Some(ActiveCapture {
                config: CaptureConfig::default(),
                input_sample_rate: TARGET_SAMPLE_RATE,
                shared,
            }),
            recording: true,
        };
        assert_eq!(capture.state(), CaptureState::Capturing);
        assert!(capture.active.is_some());
        capture.active = None;
        capture.recording = false;
        assert_eq!(capture.state(), CaptureState::Idle);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn armed_stream_is_idle_until_recording_starts() {
        let shared = Arc::new(Mutex::new(SharedCapture::default()));
        let capture = CpalAudioCapture {
            active: Some(ActiveCapture {
                config: CaptureConfig::default(),
                input_sample_rate: TARGET_SAMPLE_RATE,
                shared,
            }),
            recording: false,
        };
        // Armed but not recording: lifecycle still observes Idle and stop fails.
        assert_eq!(capture.state(), CaptureState::Idle);
        assert!(!capture.is_recording());
    }

    #[test]
    fn preroll_buffer_disabled_when_capacity_zero() {
        let mut ring = PrerollBuffer::with_capacity(0);
        ring.extend(&[1.0, 2.0, 3.0]);
        assert_eq!(ring.capacity(), 0);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert!(ring.drain().is_empty());
    }

    #[test]
    fn preroll_buffer_retains_partial_window_in_order() {
        let mut ring = PrerollBuffer::with_capacity(4);
        ring.extend(&[1.0, 2.0]);
        assert_eq!(ring.len(), 2);
        assert_eq!(ring.snapshot(), vec![1.0, 2.0]);
        assert_eq!(ring.drain(), vec![1.0, 2.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn preroll_buffer_evicts_oldest_when_full() {
        let mut ring = PrerollBuffer::with_capacity(3);
        ring.extend(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        // Only the latest three samples survive, oldest first.
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.capacity(), 3);
        assert_eq!(ring.snapshot(), vec![3.0, 4.0, 5.0]);
        assert_eq!(ring.drain(), vec![3.0, 4.0, 5.0]);
    }

    #[test]
    fn preroll_buffer_never_exceeds_capacity_across_pushes() {
        let mut ring = PrerollBuffer::with_capacity(2);
        for value in 0..1_000 {
            ring.extend(&[value as f32]);
            assert!(ring.len() <= ring.capacity());
        }
        assert_eq!(ring.snapshot(), vec![998.0, 999.0]);
    }

    #[test]
    fn preroll_buffer_wraps_when_extended_in_chunks() {
        let mut ring = PrerollBuffer::with_capacity(3);
        ring.extend(&[1.0, 2.0, 3.0]);
        ring.extend(&[4.0]);
        ring.extend(&[5.0, 6.0]);
        assert_eq!(ring.snapshot(), vec![4.0, 5.0, 6.0]);
    }

    #[test]
    fn preroll_buffer_sizes_from_duration_and_rate() {
        let ring = PrerollBuffer::new(Duration::from_millis(300), 48_000);
        assert_eq!(ring.capacity(), 14_400);
        let disabled = PrerollBuffer::new(Duration::ZERO, 48_000);
        assert_eq!(disabled.capacity(), 0);
    }

    #[test]
    fn begin_recording_prepends_preroll_and_is_idempotent() {
        let mut shared = SharedCapture::armed(PrerollBuffer::with_capacity(3));
        shared.preroll.extend(&[0.1, 0.2, 0.3]);
        shared.begin_recording();
        // Preroll becomes the head of the recording, ring is drained.
        assert_eq!(shared.samples, vec![0.1, 0.2, 0.3]);
        assert!(shared.recording);
        assert!(shared.preroll.is_empty());
        // Subsequent in-stream samples append after the preroll.
        shared.samples.extend([0.4, 0.5]);
        // A second begin_recording must not wipe accumulated samples.
        shared.begin_recording();
        assert_eq!(shared.samples, vec![0.1, 0.2, 0.3, 0.4, 0.5]);
    }

    #[test]
    fn begin_recording_without_preroll_starts_empty() {
        let mut shared = SharedCapture::armed(PrerollBuffer::with_capacity(0));
        shared.preroll.extend(&[0.9; 10]);
        shared.begin_recording();
        assert!(shared.samples.is_empty());
        assert!(shared.recording);
    }
}

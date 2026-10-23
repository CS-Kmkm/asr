//! Microphone capture, deterministic audio preprocessing, and WAV artifact creation.
//!
//! The CPAL backend is intentionally Windows-only. This lets non-Windows CI exercise
//! the DSP and lifecycle code without requiring native ALSA development packages.

mod dsp;
mod vad;
mod wav;

use dsp::resample_linear_owned;
pub use dsp::{enhance_audio, interleaved_to_mono, meter, resample_linear};
pub use vad::{EnergyVad, EnergyVadConfig, VadAnalysis, VoiceActivityDetector};
use wav::{temporary_wav_path, write_pcm16_wav};

use serde::Serialize;
use std::fmt;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
#[cfg(target_os = "windows")]
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(target_os = "windows")]
use std::thread;
use std::time::Duration;

pub const TARGET_SAMPLE_RATE: u32 = 24_000;

/// Prefix used for persisted device selections. CPAL does not expose the
/// underlying WASAPI endpoint ID, so the friendly name is the most stable
/// identifier available through its public API. In particular, unlike the old
/// `<enumeration index>:<name>` format, this survives device reordering after a
/// reboot or Bluetooth reconnect.
const DEVICE_NAME_ID_PREFIX: &str = "name:";

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
    /// Copy current audio without stopping or consuming the recording buffer.
    fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError>;
    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact>;
    fn cancel(&mut self) -> AudioFuture<'_, ()>;
    fn state(&self) -> CaptureState;
    fn level(&self) -> LevelMeter;
    fn stream_error(&self) -> Option<String> {
        None
    }
    /// Whether the open stream uses the default input device because the
    /// requested one was missing (see `CaptureConfig::fallback_to_default`).
    fn uses_fallback_device(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioDevice {
    /// Stable across re-enumeration while the device's friendly name is unchanged.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub device_id: Option<String>,
    /// Open the default input device when `device_id` names a device that is
    /// not present (for example a disconnected Bluetooth headset). Recordings
    /// opt in; the Settings microphone test does not, because it tests the
    /// chosen device. The saved selection is never changed.
    pub fallback_to_default: bool,
    pub target_sample_rate: u32,
    pub minimum_duration: Duration,
    /// Amount of pre-press audio retained in the preroll ring. `Duration::ZERO`
    /// disables prerolling and matches the legacy "open on press" behaviour.
    pub preroll: Duration,
    pub vad: EnergyVadConfig,
    pub enhancement: AudioEnhancementConfig,
    pub artifact_directory: Option<PathBuf>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            fallback_to_default: false,
            target_sample_rate: TARGET_SAMPLE_RATE,
            minimum_duration: Duration::from_millis(250),
            preroll: DEFAULT_PREROLL,
            vad: EnergyVadConfig::default(),
            enhancement: AudioEnhancementConfig::default(),
            artifact_directory: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoiseSuppressionLevel {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioEnhancementConfig {
    pub noise_suppression: NoiseSuppressionLevel,
    /// User-selected linear gain. 1.0 means no manual gain change.
    pub gain: f32,
    pub automatic_gain: bool,
}

impl Default for AudioEnhancementConfig {
    fn default() -> Self {
        Self {
            noise_suppression: NoiseSuppressionLevel::Medium,
            gain: 1.0,
            automatic_gain: true,
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
    /// Set when the input stream failed during the take. The artifact holds
    /// the audio captured before the failure.
    pub warning: Option<String>,
}

pub struct AudioSnapshot {
    samples: Vec<f32>,
    input_sample_rate: u32,
    config: CaptureConfig,
}

impl AudioSnapshot {
    /// Processing and disk I/O run outside the capture lock and callback.
    pub fn into_artifact(self) -> Result<AudioArtifact, AudioError> {
        self.prepare()?.into_artifact()
    }

    pub(crate) fn prepare(self) -> Result<PreparedAudio, AudioError> {
        prepare_samples(self.samples, self.input_sample_rate, self.config)
    }

    #[cfg(test)]
    pub(crate) fn for_test(samples: Vec<f32>, config: CaptureConfig) -> Self {
        Self {
            samples,
            input_sample_rate: TARGET_SAMPLE_RATE,
            config,
        }
    }
}

/// Enhanced audio shared by endpoint analysis and recognition. Applying gain
/// once keeps quiet-speech endpoint decisions consistent with ASR input.
pub(crate) struct PreparedAudio {
    pub(crate) samples: Vec<f32>,
    pub(crate) config: CaptureConfig,
}

impl PreparedAudio {
    pub(crate) fn duration(&self) -> Duration {
        samples_to_duration(self.samples.len(), self.config.target_sample_rate)
    }

    pub(crate) fn into_artifact(self) -> Result<AudioArtifact, AudioError> {
        write_prepared_audio(self.samples, &self.config)
    }
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
        if self.capacity == 0 || samples.is_empty() {
            return;
        }

        // A callback can occasionally deliver more than the entire window.
        // Keep its tail directly instead of rotating through the ring sample by sample.
        if samples.len() >= self.capacity {
            self.buffer.clear();
            self.buffer
                .extend_from_slice(&samples[samples.len() - self.capacity..]);
            self.head = 0;
            self.full = true;
            return;
        }

        let mut remaining = samples;
        if !self.full {
            let append = remaining.len().min(self.capacity - self.buffer.len());
            self.buffer.extend_from_slice(&remaining[..append]);
            remaining = &remaining[append..];
            if self.buffer.len() == self.capacity {
                self.full = true;
                self.head = 0;
            }
            if remaining.is_empty() {
                return;
            }
        }

        let first = remaining.len().min(self.capacity - self.head);
        self.buffer[self.head..self.head + first].copy_from_slice(&remaining[..first]);
        self.head = (self.head + first) % self.capacity;
        remaining = &remaining[first..];
        if !remaining.is_empty() {
            self.buffer[self.head..self.head + remaining.len()].copy_from_slice(remaining);
            self.head += remaining.len();
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
        if self.full && self.head != 0 {
            self.buffer.rotate_left(self.head);
        }
        let out = std::mem::take(&mut self.buffer);
        self.head = 0;
        self.full = false;
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

    /// Ends the take and hands over its samples. A stream failure (for example
    /// an unplugged or powered-off headset) keeps the audio captured before it
    /// and is returned alongside as a warning; the take fails only when the
    /// stream failed before any sample was captured.
    fn finish_recording(&mut self) -> Result<(Vec<f32>, Option<String>), AudioError> {
        self.recording = false;
        let samples = std::mem::take(&mut self.samples);
        match self.stream_error.clone() {
            Some(message) if samples.is_empty() => Err(AudioError::StreamFailure(message)),
            stream_error => Ok((samples, stream_error)),
        }
    }
}

/// An open input stream that is either armed (preroll only) or recording.
struct ActiveCapture {
    config: CaptureConfig,
    input_sample_rate: u32,
    /// The stream uses the default device in place of a missing one.
    fell_back_to_default: bool,
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
        let (mono, stream_error) = shared.finish_recording()?;
        drop(shared);
        let mut artifact = finalize_samples(mono, active.input_sample_rate, &active.config)?;
        artifact.warning = stream_error;
        Ok(artifact)
    }
}

impl AudioCapture for CpalAudioCapture {
    fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
        if !self.is_recording() {
            return Err(AudioError::NotCapturing);
        }
        let active = self.active.as_ref().ok_or(AudioError::NotCapturing)?;
        let shared = active
            .shared
            .lock()
            .map_err(|_| AudioError::StreamFailure("capture buffer lock poisoned".into()))?;
        if let Some(message) = &shared.stream_error {
            return Err(AudioError::StreamFailure(message.clone()));
        }
        Ok(AudioSnapshot {
            samples: shared.samples[((since.as_secs_f64() * f64::from(active.input_sample_rate))
                .round() as usize)
                .min(shared.samples.len())..]
                .to_vec(),
            input_sample_rate: active.input_sample_rate,
            config: active.config.clone(),
        })
    }

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
                Some(mut active) => {
                    // Processing-only settings can change while the stream is
                    // armed. Use the latest values without reopening the device.
                    active.config.enhancement = config.enhancement;
                    active
                }
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

    fn uses_fallback_device(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| active.fell_back_to_default)
    }

    fn stream_error(&self) -> Option<String> {
        self.active.as_ref().and_then(|active| {
            active
                .shared
                .lock()
                .ok()
                .and_then(|shared| shared.stream_error.clone())
        })
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
    if !config.enhancement.gain.is_finite() || !(0.25..=4.0).contains(&config.enhancement.gain) {
        return Err(AudioError::InvalidConfiguration(
            "input gain must be finite and between 0.25 and 4.0",
        ));
    }
    Ok(())
}

fn finalize_samples(
    mono_samples: Vec<f32>,
    input_sample_rate: u32,
    config: &CaptureConfig,
) -> Result<AudioArtifact, AudioError> {
    prepare_samples(mono_samples, input_sample_rate, config.clone())?.into_artifact()
}

fn prepare_samples(
    mono_samples: Vec<f32>,
    input_sample_rate: u32,
    config: CaptureConfig,
) -> Result<PreparedAudio, AudioError> {
    let mut samples =
        resample_linear_owned(mono_samples, input_sample_rate, config.target_sample_rate);
    let duration = samples_to_duration(samples.len(), config.target_sample_rate);
    if duration < config.minimum_duration {
        return Err(AudioError::TooShort {
            actual: duration,
            minimum: config.minimum_duration,
        });
    }

    enhance_audio(&mut samples, config.target_sample_rate, config.enhancement);
    Ok(PreparedAudio { samples, config })
}

fn write_prepared_audio(
    samples: Vec<f32>,
    config: &CaptureConfig,
) -> Result<AudioArtifact, AudioError> {
    let duration = samples_to_duration(samples.len(), config.target_sample_rate);
    if duration < config.minimum_duration {
        return Err(AudioError::TooShort {
            actual: duration,
            minimum: config.minimum_duration,
        });
    }

    let (vad, levels) =
        EnergyVad::new(config.vad).analyze_with_meter(&samples, config.target_sample_rate);
    if !vad.contains_voice {
        return Err(AudioError::NoVoiceDetected);
    }

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
        warning: None,
    })
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

fn device_id_from_name(name: &str) -> String {
    format!("{DEVICE_NAME_ID_PREFIX}{name}")
}

/// Resolves both current name-based IDs and the index-based IDs written by
/// versions before the name-based format was introduced. The numeric portion
/// of a legacy ID is deliberately ignored: CPAL's enumeration order is not
/// stable across device reconnects.
fn device_name_from_id(id: &str) -> Option<&str> {
    if let Some(name) = id.strip_prefix(DEVICE_NAME_ID_PREFIX) {
        return (!name.is_empty()).then_some(name);
    }

    let (index, name) = id.split_once(':')?;
    (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()) && !name.is_empty())
        .then_some(name)
}

/// The input device a capture opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
enum InputDeviceChoice<'a> {
    /// The present device with this friendly name.
    Named(&'a str),
    /// The system default input device. `fell_back` is set when it replaces
    /// a requested device that is not present.
    Default { fell_back: bool },
}

/// Chooses the input device for `requested_id` among the present devices'
/// friendly names. No request means the default device. A request that is
/// absent (or not a device id) uses the default device only when
/// `fallback_to_default` allows it; otherwise, and when there is no default
/// device, it is `DeviceNotFound`.
fn choose_input_device<'a>(
    requested_id: Option<&str>,
    available_names: &[&'a str],
    has_default: bool,
    fallback_to_default: bool,
) -> Result<InputDeviceChoice<'a>, AudioError> {
    let Some(requested_id) = requested_id else {
        return if has_default {
            Ok(InputDeviceChoice::Default { fell_back: false })
        } else {
            Err(AudioError::DeviceNotFound("default".into()))
        };
    };
    if let Some(name) = device_name_from_id(requested_id).and_then(|requested| {
        available_names
            .iter()
            .copied()
            .find(|name| *name == requested)
    }) {
        return Ok(InputDeviceChoice::Named(name));
    }
    if fallback_to_default && has_default {
        Ok(InputDeviceChoice::Default { fell_back: true })
    } else {
        Err(AudioError::DeviceNotFound(requested_id.to_owned()))
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
        .map(|device| {
            let name = device
                .name()
                .map_err(|error| AudioError::DeviceEnumeration(error.to_string()))?;
            Ok(AudioDevice {
                id: device_id_from_name(&name),
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
                Ok(CpalStartup {
                    input_sample_rate,
                    fell_back_to_default,
                    shared,
                    stream,
                }) => {
                    if startup_tx
                        .send(Ok((input_sample_rate, fell_back_to_default, shared)))
                        .is_err()
                    {
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

    let (input_sample_rate, fell_back_to_default, shared) = match startup_rx.recv() {
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
        fell_back_to_default,
        shared,
        stream_worker: StreamWorker {
            shutdown: shutdown_tx,
            thread: Some(thread),
        },
    })
}

/// What the stream thread reports once the input stream plays.
#[cfg(target_os = "windows")]
struct CpalStartup {
    input_sample_rate: u32,
    fell_back_to_default: bool,
    shared: Arc<Mutex<SharedCapture>>,
    stream: cpal::Stream,
}

#[cfg(target_os = "windows")]
fn start_cpal_stream(config: CaptureConfig) -> Result<CpalStartup, AudioError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let default_device = host.default_input_device();
    // Only a named request needs the device list.
    let mut devices: Vec<(String, cpal::Device)> = if config.device_id.is_some() {
        host.input_devices()
            .map_err(|error| AudioError::DeviceEnumeration(error.to_string()))?
            .filter_map(|device| Some((device.name().ok()?, device)))
            .collect()
    } else {
        Vec::new()
    };
    let names: Vec<&str> = devices.iter().map(|(name, _)| name.as_str()).collect();
    let choice = choose_input_device(
        config.device_id.as_deref(),
        &names,
        default_device.is_some(),
        config.fallback_to_default,
    )?;
    let (device, fell_back_to_default) = match choice {
        InputDeviceChoice::Named(name) => {
            let index = names
                .iter()
                .position(|candidate| *candidate == name)
                .expect("the chosen name is one of the listed devices");
            (devices.swap_remove(index).1, false)
        }
        InputDeviceChoice::Default { fell_back } => (
            default_device.ok_or_else(|| AudioError::DeviceNotFound("default".into()))?,
            fell_back,
        ),
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

    Ok(CpalStartup {
        input_sample_rate,
        fell_back_to_default,
        shared,
        stream,
    })
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
                // Convert and downmix directly into the mono buffer. The old
                // path first allocated a full interleaved f32 copy on every
                // callback, briefly doubling callback memory and copy work.
                let channel_count = channels as usize;
                let mono: Vec<f32> = if channel_count == 0 {
                    Vec::new()
                } else {
                    data.chunks_exact(channel_count)
                        .map(|frame| {
                            frame
                                .iter()
                                .copied()
                                .map(|sample| cpal::Sample::to_sample::<f32>(sample))
                                .sum::<f32>()
                                / channel_count as f32
                        })
                        .collect()
                };
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

    #[tokio::test]
    async fn snapshot_keeps_capture_active_and_final_audio_complete() {
        let directory = tempfile::tempdir().unwrap();
        let config = CaptureConfig {
            artifact_directory: Some(directory.path().to_owned()),
            ..CaptureConfig::default()
        };
        let samples: Vec<f32> = (0..24_000).map(|i| (i as f32 * 0.1).sin() * 0.2).collect();
        let shared = Arc::new(Mutex::new(SharedCapture {
            samples: samples.clone(),
            recording: true,
            ..SharedCapture::default()
        }));
        let mut capture = CpalAudioCapture {
            recording: true,
            active: Some(ActiveCapture {
                config,
                input_sample_rate: TARGET_SAMPLE_RATE,
                fell_back_to_default: false,
                shared: Arc::clone(&shared),
                #[cfg(target_os = "windows")]
                stream_worker: StreamWorker {
                    shutdown: mpsc::channel().0,
                    thread: None,
                },
            }),
        };
        let snapshot = capture.snapshot(Duration::ZERO).unwrap();
        shared.lock().unwrap().samples.extend_from_slice(&samples);
        assert_eq!(
            capture
                .snapshot(Duration::from_secs(1))
                .unwrap()
                .samples
                .len(),
            24_000
        );
        let partial = snapshot.into_artifact().unwrap();
        assert_eq!(partial.sample_count, 24_000);
        assert_eq!(capture.state(), CaptureState::Capturing);
        assert!(shared.lock().unwrap().recording);
        let complete = capture.stop().await.unwrap();
        assert_eq!(complete.sample_count, 48_000);
        assert_eq!(capture.state(), CaptureState::Idle);
        assert_ne!(partial.path, complete.path);
        partial.remove().unwrap();
        complete.remove().unwrap();
    }

    fn recording_capture(shared: &Arc<Mutex<SharedCapture>>, directory: &Path) -> CpalAudioCapture {
        CpalAudioCapture {
            recording: true,
            active: Some(ActiveCapture {
                config: CaptureConfig {
                    artifact_directory: Some(directory.to_owned()),
                    ..CaptureConfig::default()
                },
                input_sample_rate: TARGET_SAMPLE_RATE,
                fell_back_to_default: false,
                shared: Arc::clone(shared),
                #[cfg(target_os = "windows")]
                stream_worker: StreamWorker {
                    shutdown: mpsc::channel().0,
                    thread: None,
                },
            }),
        }
    }

    #[tokio::test]
    async fn stream_failure_keeps_the_audio_captured_before_it() {
        let directory = tempfile::tempdir().unwrap();
        let samples: Vec<f32> = (0..24_000).map(|i| (i as f32 * 0.1).sin() * 0.2).collect();
        let shared = Arc::new(Mutex::new(SharedCapture {
            samples,
            recording: true,
            stream_error: Some("device disconnected".into()),
            ..SharedCapture::default()
        }));
        let mut capture = recording_capture(&shared, directory.path());
        let artifact = capture.stop().await.unwrap();
        assert_eq!(artifact.sample_count, 24_000);
        assert_eq!(artifact.warning.as_deref(), Some("device disconnected"));
        assert!(artifact.path.is_file());
        assert_eq!(capture.state(), CaptureState::Idle);
        artifact.remove().unwrap();
    }

    #[tokio::test]
    async fn stream_failure_before_any_audio_still_fails_the_take() {
        let directory = tempfile::tempdir().unwrap();
        let shared = Arc::new(Mutex::new(SharedCapture {
            recording: true,
            stream_error: Some("device disconnected".into()),
            ..SharedCapture::default()
        }));
        let mut capture = recording_capture(&shared, directory.path());
        assert!(matches!(
            capture.stop().await,
            Err(AudioError::StreamFailure(message)) if message == "device disconnected"
        ));
        assert_eq!(capture.state(), CaptureState::Idle);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_healthy_take_finishes_without_a_warning() {
        let mut shared = SharedCapture {
            samples: vec![0.1; 8],
            recording: true,
            ..SharedCapture::default()
        };
        let (samples, warning) = shared.finish_recording().unwrap();
        assert_eq!(samples.len(), 8);
        assert_eq!(warning, None);
        assert!(!shared.recording);
        assert!(shared.samples.is_empty());
    }

    #[test]
    fn device_ids_do_not_depend_on_enumeration_order() {
        assert_eq!(
            device_id_from_name("ヘッドセット (SOUNDPEATS Air5 Pro)"),
            "name:ヘッドセット (SOUNDPEATS Air5 Pro)"
        );
    }

    #[test]
    fn legacy_device_ids_resolve_by_name_after_reordering() {
        assert_eq!(
            device_name_from_id("2:ヘッドセット (SOUNDPEATS Air5 Pro)"),
            Some("ヘッドセット (SOUNDPEATS Air5 Pro)")
        );
        assert_eq!(
            device_name_from_id("name:Microphone: USB"),
            Some("Microphone: USB")
        );
        assert_eq!(device_name_from_id("not-an-id"), None);
    }

    #[test]
    fn no_saved_microphone_opens_the_default_device() {
        let present = ["USB Microphone"];
        for fallback in [false, true] {
            assert_eq!(
                choose_input_device(None, &present, true, fallback).unwrap(),
                InputDeviceChoice::Default { fell_back: false }
            );
        }
    }

    #[test]
    fn a_present_saved_microphone_is_used() {
        let present = ["USB Microphone", "ヘッドセット (SOUNDPEATS Air5 Pro)"];
        for fallback in [false, true] {
            assert_eq!(
                choose_input_device(
                    Some("name:ヘッドセット (SOUNDPEATS Air5 Pro)"),
                    &present,
                    true,
                    fallback,
                )
                .unwrap(),
                InputDeviceChoice::Named("ヘッドセット (SOUNDPEATS Air5 Pro)")
            );
            // A legacy index-based id resolves by name too.
            assert_eq!(
                choose_input_device(Some("7:USB Microphone"), &present, false, fallback).unwrap(),
                InputDeviceChoice::Named("USB Microphone")
            );
        }
    }

    #[test]
    fn a_missing_saved_microphone_falls_back_to_the_default_when_allowed() {
        let present = ["USB Microphone"];
        for missing in [
            "name:ヘッドセット (SOUNDPEATS Air5 Pro)",
            "not-an-id",
            "name:",
        ] {
            assert_eq!(
                choose_input_device(Some(missing), &present, true, true).unwrap(),
                InputDeviceChoice::Default { fell_back: true },
                "{missing}"
            );
        }
        assert_eq!(
            choose_input_device(Some("name:Headset"), &[], true, true).unwrap(),
            InputDeviceChoice::Default { fell_back: true }
        );
    }

    #[test]
    fn without_fallback_a_missing_saved_microphone_is_not_found() {
        // The Settings microphone test tests the chosen device; it must not
        // silently measure another one.
        let present = ["USB Microphone"];
        for missing in ["name:Headset", "not-an-id"] {
            let error = choose_input_device(Some(missing), &present, true, false).unwrap_err();
            assert!(matches!(&error, AudioError::DeviceNotFound(id) if id == missing));
            assert!(error
                .to_string()
                .starts_with("audio input device not found"));
        }
    }

    #[test]
    fn no_default_device_is_not_found_even_with_fallback() {
        let present = ["USB Microphone"];
        assert!(matches!(
            choose_input_device(None, &present, false, true),
            Err(AudioError::DeviceNotFound(id)) if id == "default"
        ));
        assert!(matches!(
            choose_input_device(Some("name:Headset"), &present, false, true),
            Err(AudioError::DeviceNotFound(id)) if id == "name:Headset"
        ));
    }

    #[test]
    fn captures_do_not_fall_back_unless_they_opt_in() {
        assert!(!CaptureConfig::default().fallback_to_default);
        let capture = CpalAudioCapture::new();
        assert!(!capture.uses_fallback_device());
    }

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
    fn owned_resampling_matches_borrowed_resampling() {
        let source: Vec<f32> = (0..997).map(|index| (index as f32 * 0.17).sin()).collect();
        for target_rate in [16_000, 24_000, 48_000] {
            let expected = resample_linear(&source, 48_000, target_rate);
            let actual = resample_linear_owned(source.clone(), 48_000, target_rate);
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn combined_vad_meter_matches_standalone_meter() {
        let samples = [0.0, 0.25, -0.5, f32::NAN, 2.0, 0.0];
        let (_, combined) = EnergyVad::new(EnergyVadConfig::default())
            .analyze_with_meter(&samples, TARGET_SAMPLE_RATE);
        assert_eq!(combined, meter(&samples));
    }

    #[test]
    fn manual_gain_increases_quiet_audio_without_clipping() {
        let mut samples: Vec<f32> = (0..2_400)
            .map(|index| (index as f32 * 0.07).sin() * 0.05)
            .collect();
        let before = meter(&samples).rms;
        enhance_audio(
            &mut samples,
            TARGET_SAMPLE_RATE,
            AudioEnhancementConfig {
                noise_suppression: NoiseSuppressionLevel::Off,
                gain: 2.0,
                automatic_gain: false,
            },
        );
        let after = meter(&samples);
        assert!(after.rms > before * 1.7);
        assert!(after.peak <= 1.0);
        assert!(samples.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn high_noise_suppression_attenuates_quiet_frames() {
        let noise: Vec<f32> = (0..2_400)
            .map(|index| if index % 2 == 0 { 0.006 } else { -0.006 })
            .collect();
        let speech: Vec<f32> = (0..2_400)
            .map(|index| (index as f32 * 0.11).sin() * 0.12)
            .collect();
        let mut off = [noise.clone(), speech.clone()].concat();
        let mut high = off.clone();
        enhance_audio(
            &mut off,
            TARGET_SAMPLE_RATE,
            AudioEnhancementConfig {
                noise_suppression: NoiseSuppressionLevel::Off,
                gain: 1.0,
                automatic_gain: false,
            },
        );
        enhance_audio(
            &mut high,
            TARGET_SAMPLE_RATE,
            AudioEnhancementConfig {
                noise_suppression: NoiseSuppressionLevel::High,
                gain: 1.0,
                automatic_gain: false,
            },
        );
        let quiet_tail = 1_200..2_400;
        assert!(meter(&high[quiet_tail.clone()]).rms < meter(&off[quiet_tail]).rms * 0.5);
        assert!(meter(&high[2_640..]).rms > meter(&off[2_640..]).rms * 0.8);
    }

    #[test]
    fn enhancement_sanitizes_non_finite_samples() {
        let mut samples = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.25];
        enhance_audio(
            &mut samples,
            TARGET_SAMPLE_RATE,
            AudioEnhancementConfig::default(),
        );
        assert!(samples.iter().all(|sample| sample.is_finite()));
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
                fell_back_to_default: false,
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
                fell_back_to_default: false,
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

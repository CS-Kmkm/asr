//! Opt-in incremental final recognition for Dictate and Translate.
//!
//! While recording, the live loop confirms range boundaries in long pauses
//! and recognizes each confirmed range with the final-recognition request
//! (never the preview decoding). At stop only the rest of the recording is
//! recognized, and the texts are joined in audio order. Nothing is ever
//! stitched by looking for repeated words: a range is a disjoint span of raw
//! samples, so the join is a concatenation.
//!
//! Decisions (audit item A5):
//! - Boundaries: a range ends in the middle of a run of at least
//!   [`BOUNDARY_SILENCE`] in which every 20 ms frame of the prepared audio
//!   stays below the live-segmentation continuation level (0.2 x the VAD
//!   threshold), so each side keeps at least half of that pause. A range
//!   holds at least [`MIN_RANGE`] of audio and voice of at least the VAD's
//!   minimum (or no voice at all); among the cuts available, the latest that
//!   keeps the range within [`MAX_RANGE`] wins. Continuous speech without
//!   such a pause is never cut; it stays in the part recognized at stop.
//! - Preparation: in this mode the stop path prepares each range on its own:
//!   the raw samples of a range are resampled and enhanced as one window
//!   (noise floor and automatic gain from that range), and the tail at stop
//!   likewise. Both use the take's capture configuration, which is checked.
//!   Whole-recording statistics cannot be known before the take ends, so per
//!   range preparation is the definition, and it is why results may differ
//!   slightly from whole-recording recognition. Fallbacks keep the
//!   unchanged whole-recording preparation.
//! - Scheduling: the live loop is serial, so a range decode and a preview
//!   decode never overlap. A confirmed range is decoded in the same turn;
//!   with preview on, that turn skips the preview and the next turn always
//!   goes to the preview, so neither starves the other. Cancelling the take
//!   cancels a range decode in flight; no range is queued.
//! - Stop: a range not yet started when stop is signalled is never started;
//!   its audio joins the tail. A range decode in flight is awaited by the
//!   existing live drain (as a preview decode was), which by construction
//!   takes less than recognizing that range again, and its result is used.
//! - Prompt: every range and the tail send exactly the final request (the
//!   dictionary prompt, locale and default decoding of the stop path). No
//!   text of an earlier range is passed on, so a range never depends on the
//!   hypothesis of another one.
//! - Providers: only local backends. With `openai-compatible` the setting is
//!   ignored and the take falls back (`cloud_backend`), sending nothing new.
//!
//! Results are reused only if the recognition identity (backend, model,
//! quantization, device, locale, the setting itself), the prompt, the
//! resident model generation and the preparation configuration are the ones
//! of the stop path, every range succeeded, every range or tail judged
//! silent is below the VAD threshold in the prepared whole recording too,
//! and the ranges and tail cover the recording exactly. Otherwise the whole recording is recognized as
//! before, and a content-free metric records the reason.
use super::segmentation::{append_utterance, FRAME, VOICE_HYSTERESIS};
use crate::{
    asr::{AsrError, ModelResidency, Segment, Transcriber, Transcript},
    audio::{self, AudioCapture, AudioError, AudioSnapshot, CaptureConfig, FinalTail},
    storage::MetricRecorder,
    types::{AsrDevice, Settings},
    TempArtifact,
};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Metric event of an incremental attempt: `success` means the joined
/// result was used; otherwise `error_code` is a [`Fallback`] code.
pub(crate) const METRIC_EVENT: &str = "asr_incremental";

/// Pause in which a range boundary may be confirmed.
const BOUNDARY_SILENCE: Duration = Duration::from_millis(1_000);
/// Shortest range: Whisper pads short windows, so tiny ranges cost more.
const MIN_RANGE: Duration = Duration::from_secs(5);
/// Preferred longest range: one Whisper window.
const MAX_RANGE: Duration = Duration::from_secs(28);
/// New audio needed before the pending audio is analysed again.
const ANALYSIS_STEP: Duration = Duration::from_millis(1_000);

/// Why a take was recognized as a whole recording after all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// The speech backend is a cloud provider.
    CloudBackend,
    /// No range was confirmed before stop.
    NoRanges,
    /// Recognition settings or the setting itself changed during the take.
    SettingsChanged,
    /// The dictionary prompt at stop differs from the one ranges used.
    PromptChanged,
    /// The worker reloaded or restored the model, or reported another model.
    ModelChanged,
    /// The transcriber cannot report which model generation decoded.
    ModelUnverified,
    /// A range decode failed.
    RangeFailed,
    /// A range decode was cancelled (for example by a worker restart).
    RangeCancelled,
    /// The ranges and the tail do not cover the recording exactly.
    MissingCoverage,
    /// Audio was prepared with another capture configuration.
    PreparationChanged,
    /// Preparing a range or the tail failed.
    PreparationFailed,
    /// A range or the tail prepared on its own showed no voice, but the
    /// prepared whole recording reaches the VAD threshold there, so the
    /// whole-recording request would decode that sound.
    VoiceMismatch,
    /// Recognizing the tail failed.
    TailFailed,
    /// The join is empty although the recording has voice.
    EmptyResult,
}

impl Fallback {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::CloudBackend => "cloud_backend",
            Self::NoRanges => "no_ranges",
            Self::SettingsChanged => "settings_changed",
            Self::PromptChanged => "prompt_changed",
            Self::ModelChanged => "model_changed",
            Self::ModelUnverified => "model_unverified",
            Self::RangeFailed => "range_failed",
            Self::RangeCancelled => "range_cancelled",
            Self::MissingCoverage => "missing_coverage",
            Self::PreparationChanged => "preparation_changed",
            Self::PreparationFailed => "preparation_failed",
            Self::VoiceMismatch => "voice_mismatch",
            Self::TailFailed => "tail_failed",
            Self::EmptyResult => "empty_result",
        }
    }
}

/// Settings whose change makes recognized ranges stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecognitionIdentity {
    enabled: bool,
    backend: String,
    model: Option<String>,
    quantization: String,
    device: Option<AsrDevice>,
    locale: Option<String>,
}

impl RecognitionIdentity {
    fn of(settings: &Settings) -> Self {
        Self {
            enabled: settings.incremental_final_asr,
            backend: settings.asr_backend.clone(),
            model: crate::model_identity(settings).0,
            quantization: settings.model_quantization.clone(),
            device: settings.asr_device,
            locale: settings.speech_locale.clone(),
        }
    }
}

fn local_backend(backend: &str) -> bool {
    backend != "openai-compatible"
}

/// A take's incremental recognition, from recording start to stop.
pub(crate) enum Take {
    /// Enabled, but this take cannot use it.
    Unavailable(Fallback),
    /// Enabled; recording has not begun recognizing ranges yet.
    Planned(RecognitionIdentity),
    /// Ranges recognized while recording.
    Recorded(Box<Session>),
}

impl Take {
    /// The take's plan when the setting is on; `None` keeps every step of
    /// the take exactly as without this feature.
    pub(crate) fn plan(settings: &Settings) -> Option<Self> {
        if !settings.incremental_final_asr {
            return None;
        }
        Some(if local_backend(&settings.asr_backend) {
            Self::Planned(RecognitionIdentity::of(settings))
        } else {
            Self::Unavailable(Fallback::CloudBackend)
        })
    }

    /// Starts recognizing ranges with the live requests' prompt and locale.
    pub(crate) fn begin(self, prompt: Option<&str>, locale: Option<&str>) -> Self {
        match self {
            Self::Planned(identity) => Self::Recorded(Box::new(Session::new(
                identity,
                prompt.map(str::to_owned),
                locale.map(str::to_owned),
            ))),
            other => other,
        }
    }

    pub(crate) fn session_mut(&mut self) -> Option<&mut Session> {
        match self {
            Self::Recorded(session) => Some(session),
            _ => None,
        }
    }
}

/// A recognized range: raw samples `start..end` of the recording.
struct Range {
    start: usize,
    end: usize,
    /// `None` for a range without voice, which needs no decode.
    transcript: Option<Transcript>,
    /// The resident model generation before and after its decode.
    generation: Option<u64>,
}

/// What one turn of the live loop did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Turn {
    /// Nothing that used the worker; the preview may run.
    Idle,
    /// A range decode used the worker this turn.
    Used,
    /// Stop or cancellation: end the loop.
    Stop,
    /// The microphone failed.
    CaptureFailed,
}

pub(crate) struct Session {
    identity: RecognitionIdentity,
    prompt: Option<String>,
    locale: Option<String>,
    tail: FinalTail,
    registered: bool,
    /// Raw sample where the audio not yet in a range starts.
    start: usize,
    /// Pending audio at the last analysis that found no boundary.
    analyzed: Duration,
    ranges: Vec<Range>,
    failure: Option<Fallback>,
    input_rate: Option<u32>,
    config: Option<CaptureConfig>,
    yield_turn: bool,
}

impl Session {
    fn new(identity: RecognitionIdentity, prompt: Option<String>, locale: Option<String>) -> Self {
        Self {
            identity,
            prompt,
            locale,
            tail: FinalTail::default(),
            registered: false,
            start: 0,
            analyzed: Duration::ZERO,
            ranges: Vec::new(),
            failure: None,
            input_rate: None,
            config: None,
            yield_turn: false,
        }
    }

    /// Whether ranges may still be recognized in this take.
    pub(super) fn active(&self) -> bool {
        self.failure.is_none()
    }

    fn fail(&mut self, reason: Fallback) -> Turn {
        self.failure.get_or_insert(reason);
        Turn::Idle
    }

    /// One turn: confirm at most one range and recognize it at once.
    /// `preview` makes the next turn go to the live preview after a decode.
    pub(super) async fn step(
        &mut self,
        audio: &tokio::sync::Mutex<Box<dyn AudioCapture>>,
        transcriber: &dyn Transcriber,
        cancel: &watch::Receiver<bool>,
        stopped: &watch::Receiver<bool>,
        preview: bool,
    ) -> Turn {
        let ended = || *cancel.borrow() || *stopped.borrow();
        if self.failure.is_some() || std::mem::take(&mut self.yield_turn) {
            return Turn::Idle;
        }
        let since = match self.input_rate {
            Some(rate) => audio::raw_position(self.start, rate),
            None => Duration::ZERO,
        };
        let due = |pending: Duration, analyzed: Duration| {
            pending >= MIN_RANGE + BOUNDARY_SILENCE / 2
                && pending.saturating_sub(analyzed) >= ANALYSIS_STEP.max(pending.mul_f64(0.08))
        };
        let snapshot = {
            let mut audio = audio.lock().await;
            if ended() {
                return Turn::Stop;
            }
            if !self.registered {
                self.registered = true;
                if !audio.retain_final_tail(self.tail.clone()) {
                    return self.fail(Fallback::MissingCoverage);
                }
            }
            // A probe ending far after `since` copies no samples.
            match audio.probe(since, since + Duration::from_secs(86_400)) {
                Ok(Some(probe)) if !due(probe.duration(), self.analyzed) => return Turn::Idle,
                Ok(_) => {}
                Err(AudioError::NotCapturing) => return Turn::Stop,
                Err(_) => return Turn::CaptureFailed,
            }
            match audio.snapshot(since) {
                Ok(snapshot) => snapshot,
                Err(AudioError::NotCapturing) => return Turn::Stop,
                Err(_) => return Turn::CaptureFailed,
            }
        };
        let pending = snapshot.duration();
        if !due(pending, self.analyzed) {
            return Turn::Idle;
        }
        if *self.input_rate.get_or_insert(snapshot.input_sample_rate())
            != snapshot.input_sample_rate()
            || !self
                .config
                .get_or_insert_with(|| snapshot.config().clone())
                .prepares_like(snapshot.config())
        {
            return self.fail(Fallback::PreparationChanged);
        }
        let planned = tokio::task::spawn_blocking(move || plan_range(snapshot)).await;
        let (cut, artifact) = match planned {
            Ok(Ok(Some(range))) => range,
            Ok(Ok(None)) => {
                self.analyzed = pending;
                return Turn::Idle;
            }
            Ok(Err(_)) | Err(_) => return self.fail(Fallback::PreparationFailed),
        };
        let mut cleanup = artifact
            .as_ref()
            .map(|artifact| TempArtifact::new(artifact.path.clone(), false));
        let end = self.start + cut;
        // A range not started before stop joins the tail instead.
        if ended() || !self.tail.advance(end) {
            return Turn::Stop;
        }
        let mut range = Range {
            start: self.start,
            end,
            transcript: None,
            generation: None,
        };
        self.start = end;
        self.analyzed = Duration::ZERO;
        let Some(artifact) = artifact else {
            self.ranges.push(range);
            return Turn::Idle;
        };
        let before = resident_generation(transcriber);
        let result = transcriber
            .transcribe_with_locale(
                &artifact.path,
                self.prompt.as_deref(),
                self.locale.as_deref(),
                cancel.clone(),
            )
            .await;
        let after = resident_generation(transcriber);
        if let Some(cleanup) = cleanup.as_mut() {
            let _ = cleanup.cleanup();
        }
        self.yield_turn = preview;
        match result {
            Ok(transcript) => {
                range.transcript = Some(transcript);
                range.generation = before.filter(|_| before == after);
                self.ranges.push(range);
            }
            Err(AsrError::Cancelled) => {
                self.fail(Fallback::RangeCancelled);
                if *cancel.borrow() {
                    return Turn::Stop;
                }
            }
            Err(_) => {
                self.fail(Fallback::RangeFailed);
            }
        }
        Turn::Used
    }
}

fn resident_generation(transcriber: &dyn Transcriber) -> Option<u64> {
    match *transcriber.model_residency()?.borrow() {
        ModelResidency::Resident(generation) => Some(generation),
        _ => None,
    }
}

/// Finds a boundary in the pending audio and prepares the range before it:
/// the raw cut and the range's WAV (`None` for a range without voice).
fn plan_range(
    mut snapshot: AudioSnapshot,
) -> Result<Option<(usize, Option<audio::AudioArtifact>)>, AudioError> {
    let analysis = match snapshot.clone().prepare() {
        Ok(prepared) => prepared,
        Err(AudioError::TooShort { .. }) => return Ok(None),
        Err(error) => return Err(error),
    };
    let rate = analysis.config.target_sample_rate;
    let Some(cut) = find_boundary(
        &analysis.samples,
        rate,
        analysis.config.vad.rms_threshold,
        analysis.config.vad.minimum_voice_duration,
    ) else {
        return Ok(None);
    };
    let raw_cut = ((cut as f64 * f64::from(snapshot.input_sample_rate()) / f64::from(rate)).round()
        as usize)
        .min(snapshot.raw_len());
    snapshot.truncate(raw_cut);
    let artifact = match snapshot
        .prepare()
        .and_then(|prepared| prepared.into_artifact())
    {
        Ok(artifact) => Some(artifact),
        Err(AudioError::TooShort { .. } | AudioError::NoVoiceDetected) => None,
        Err(error) => return Err(error),
    };
    Ok(Some((raw_cut, artifact)))
}

/// Whether any VAD frame of `prepared` reaches the VAD threshold.
fn reaches_vad_threshold(prepared: &[f32], config: &CaptureConfig) -> bool {
    let frame = samples(config.vad.frame_duration, config.target_sample_rate).max(1);
    let threshold = f64::from(config.vad.rms_threshold);
    prepared.chunks(frame).any(|values| {
        let mean_square =
            values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64;
        mean_square.sqrt() >= threshold
    })
}

fn samples(duration: Duration, rate: u32) -> usize {
    (duration.as_secs_f64() * f64::from(rate)).round() as usize
}

/// The prepared-sample index of a boundary in `prepared` (see the module
/// documentation), or `None`.
fn find_boundary(
    prepared: &[f32],
    rate: u32,
    vad_threshold: f32,
    minimum_voice: Duration,
) -> Option<usize> {
    let frame = samples(FRAME, rate).max(1);
    let continuation = f64::from(vad_threshold) * VOICE_HYSTERESIS;
    let needed_quiet = samples(BOUNDARY_SILENCE, rate);
    let minimum_voice = samples(minimum_voice, rate);
    let (min_range, max_range) = (samples(MIN_RANGE, rate), samples(MAX_RANGE, rate));
    let mut candidates = Vec::new();
    let mut voiced = 0;
    let mut heard = false;
    let mut quiet_start = None;
    let mut consider = |quiet_start: usize, quiet_end: usize, voiced: usize, heard: bool| {
        let cut = quiet_start + (quiet_end - quiet_start) / 2;
        if quiet_end - quiet_start >= needed_quiet
            && (voiced >= minimum_voice || !heard)
            && cut >= min_range
        {
            candidates.push(cut);
        }
    };
    for (index, values) in prepared.chunks(frame).enumerate() {
        let start = index * frame;
        let rms = (values.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / values.len() as f64)
            .sqrt();
        if rms >= continuation {
            if let Some(quiet) = quiet_start.take() {
                consider(quiet, start, voiced, heard);
            }
            heard = true;
            if rms >= f64::from(vad_threshold) {
                voiced += values.len();
            }
        } else if quiet_start.is_none() {
            quiet_start = Some(start);
        }
    }
    if let Some(quiet) = quiet_start {
        consider(quiet, prepared.len(), voiced, heard);
    }
    candidates
        .iter()
        .rev()
        .find(|cut| **cut <= max_range)
        .or(candidates.first())
        .copied()
}

/// What an attempt at stop did, for the content-free metric.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    fallback: Option<Fallback>,
    elapsed: Duration,
}

impl Outcome {
    pub(crate) fn record(&self, metrics: MetricRecorder<'_>, asr_backend: &str) {
        let _ = metrics.add_metric(
            METRIC_EVENT,
            Some(asr_backend),
            self.fallback
                .is_none()
                .then_some(self.elapsed.as_millis() as i64),
            self.fallback.is_none(),
            self.fallback.map(Fallback::code),
        );
    }
}

enum Attempt {
    Joined(Transcript),
    Fallback(Fallback),
    Cancelled,
}

/// Final recognition of a stopped Dictate or Translate take. Without an
/// incremental `take` this is exactly the whole-recording request. With
/// one, it joins the ranges and the recognized tail or falls back to that
/// request; `record` receives the outcome unless the take was cancelled.
pub(crate) async fn transcribe_final(
    transcriber: &dyn Transcriber,
    take: Option<Take>,
    settings: &Settings,
    audio_path: &Path,
    prompt: Option<&str>,
    cancel: watch::Receiver<bool>,
    record: impl FnOnce(Outcome),
) -> Result<Transcript, AsrError> {
    let locale = settings.speech_locale.as_deref();
    let Some(take) = take else {
        return transcriber
            .transcribe_with_locale(audio_path, prompt, locale, cancel)
            .await;
    };
    let started = Instant::now();
    let attempt = match take {
        Take::Unavailable(reason) => Attempt::Fallback(reason),
        Take::Planned(_) => Attempt::Fallback(Fallback::NoRanges),
        Take::Recorded(session) => {
            join_recorded(
                transcriber,
                *session,
                settings,
                audio_path,
                prompt,
                cancel.clone(),
            )
            .await
        }
    };
    let fallback = match attempt {
        Attempt::Cancelled => return Err(AsrError::Cancelled),
        Attempt::Joined(transcript) => {
            record(Outcome {
                fallback: None,
                elapsed: started.elapsed(),
            });
            return Ok(transcript);
        }
        Attempt::Fallback(reason) => reason,
    };
    let result = transcriber
        .transcribe_with_locale(audio_path, prompt, locale, cancel)
        .await;
    if !matches!(result, Err(AsrError::Cancelled)) {
        record(Outcome {
            fallback: Some(fallback),
            elapsed: started.elapsed(),
        });
    }
    result
}

async fn join_recorded(
    transcriber: &dyn Transcriber,
    session: Session,
    settings: &Settings,
    audio_path: &Path,
    prompt: Option<&str>,
    cancel: watch::Receiver<bool>,
) -> Attempt {
    use Attempt::Fallback as Fall;
    if let Some(reason) = session.failure {
        return Fall(reason);
    }
    if !local_backend(&settings.asr_backend) {
        return Fall(Fallback::CloudBackend);
    }
    if session.ranges.is_empty() {
        return Fall(Fallback::NoRanges);
    }
    if session.identity != RecognitionIdentity::of(settings) {
        return Fall(Fallback::SettingsChanged);
    }
    if session.prompt.as_deref() != prompt {
        return Fall(Fallback::PromptChanged);
    }
    let Some(residency) = transcriber.model_residency() else {
        return Fall(Fallback::ModelUnverified);
    };
    let current = match *residency.borrow() {
        ModelResidency::Resident(generation) => Some(generation),
        _ => None,
    };
    if current.is_none()
        || session
            .ranges
            .iter()
            .any(|range| range.transcript.is_some() && range.generation != current)
    {
        return Fall(Fallback::ModelChanged);
    }
    let Some(sealed) = session.tail.take() else {
        return Fall(Fallback::MissingCoverage);
    };
    let contiguous = session.ranges.first().is_some_and(|range| range.start == 0)
        && session
            .ranges
            .windows(2)
            .all(|pair| pair[0].end == pair[1].start)
        && session.ranges.last().map(|range| range.end) == Some(sealed.from);
    let Some(tail) = sealed.audio.filter(|tail| {
        contiguous && sealed.from > 0 && sealed.from + tail.raw_len() == sealed.total
    }) else {
        return Fall(Fallback::MissingCoverage);
    };
    let (Some(rate), Some(config)) = (session.input_rate, session.config.as_ref()) else {
        return Fall(Fallback::MissingCoverage);
    };
    if tail.input_sample_rate() != rate || !tail.config().prepares_like(config) {
        return Fall(Fallback::PreparationChanged);
    }
    let prepared = tokio::task::spawn_blocking(move || {
        match tail.prepare().and_then(|prepared| prepared.into_artifact()) {
            Ok(artifact) => Ok(Some(artifact)),
            Err(AudioError::TooShort { .. } | AudioError::NoVoiceDetected) => Ok(None),
            Err(error) => Err(error),
        }
    })
    .await;
    let artifact = match prepared {
        Ok(Ok(artifact)) => artifact,
        Ok(Err(_)) | Err(_) => return Fall(Fallback::PreparationFailed),
    };
    let mut cleanup = artifact
        .as_ref()
        .map(|artifact| TempArtifact::new(artifact.path.clone(), false));
    // Parts judged silent were prepared on their own; the prepared whole
    // recording, which a fallback decodes, must be silent there too.
    let silent: Vec<(usize, usize)> = session
        .ranges
        .iter()
        .filter(|range| range.transcript.is_none())
        .map(|range| (range.start, range.end))
        .chain(artifact.is_none().then_some((sealed.from, sealed.total)))
        .collect();
    if !silent.is_empty() {
        let whole = audio_path.to_owned();
        let config = config.clone();
        let checked = tokio::task::spawn_blocking(move || {
            sound_in_whole_recording(&whole, &silent, rate, &config)
        })
        .await;
        match checked {
            Ok(Ok(false)) => {}
            Ok(Ok(true)) => return Fall(Fallback::VoiceMismatch),
            Ok(Err(_)) | Err(_) => return Fall(Fallback::PreparationFailed),
        }
    }
    if *cancel.borrow() {
        return Attempt::Cancelled;
    }
    let tail_transcript = match &artifact {
        None => None,
        Some(artifact) => {
            let result = transcriber
                .transcribe_with_locale(
                    &artifact.path,
                    prompt,
                    settings.speech_locale.as_deref(),
                    cancel,
                )
                .await;
            if let Some(cleanup) = cleanup.as_mut() {
                let _ = cleanup.cleanup();
            }
            match result {
                Ok(transcript) => Some(transcript),
                Err(AsrError::Cancelled) => return Attempt::Cancelled,
                Err(_) => return Fall(Fallback::TailFailed),
            }
        }
    };
    if tail_transcript.is_some() && resident_generation(transcriber) != current {
        return Fall(Fallback::ModelChanged);
    }
    let offset = |index: usize| index as f64 / f64::from(rate);
    let parts = session
        .ranges
        .into_iter()
        .filter_map(|range| range.transcript.map(|t| (offset(range.start), t)))
        .chain(tail_transcript.map(|t| (offset(sealed.from), t)));
    match join(parts) {
        Ok(transcript) if !transcript.text.trim().is_empty() => Attempt::Joined(transcript),
        Ok(_) => Fall(Fallback::EmptyResult),
        Err(reason) => Fall(reason),
    }
}

/// Whether the prepared whole recording at `path` reaches the VAD threshold
/// in any of the raw sample spans `parts` (at `input_rate`).
fn sound_in_whole_recording(
    path: &Path,
    parts: &[(usize, usize)],
    input_rate: u32,
    config: &CaptureConfig,
) -> Result<bool, AudioError> {
    let (samples, rate) = audio::read_pcm16_wav(path)?;
    let at = |raw: usize| {
        ((raw as f64 * f64::from(rate) / f64::from(input_rate)).round() as usize).min(samples.len())
    };
    Ok(parts.iter().any(|&(start, end)| {
        let (start, end) = (at(start), at(end));
        start < end && reaches_vad_threshold(&samples[start..end], config)
    }))
}

/// Joins recognized parts, each with its start in seconds, in audio order.
/// Texts are concatenated with the live preview's language-aware spacing;
/// nothing is removed, so a word said twice stays twice.
fn join(parts: impl IntoIterator<Item = (f64, Transcript)>) -> Result<Transcript, Fallback> {
    let mut joined: Option<Transcript> = None;
    for (offset, part) in parts {
        let segments = part.segments.into_iter().map(|segment| Segment {
            start: segment.start + offset,
            end: segment.end + offset,
            ..segment
        });
        match joined.as_mut() {
            None => {
                joined = Some(Transcript {
                    text: part.text.trim().to_owned(),
                    segments: segments.collect(),
                    model: part.model,
                    duration_ms: part.duration_ms,
                });
            }
            Some(joined) => {
                if joined.model != part.model {
                    return Err(Fallback::ModelChanged);
                }
                joined.text = append_utterance(&joined.text, &part.text);
                joined.segments.extend(segments);
                joined.duration_ms += part.duration_ms;
            }
        }
    }
    joined.ok_or(Fallback::EmptyResult)
}

#[cfg(test)]
mod tests;

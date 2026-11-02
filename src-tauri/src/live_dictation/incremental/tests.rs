use super::*;
use crate::asr::WorkerCommand;
use crate::audio::{AudioDevice, AudioFuture, CaptureState, LevelMeter, TARGET_SAMPLE_RATE};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const RATE: u32 = TARGET_SAMPLE_RATE;

fn ms(milliseconds: usize) -> usize {
    milliseconds * RATE as usize / 1_000
}

fn silence(milliseconds: usize) -> Vec<f32> {
    vec![0.0; ms(milliseconds)]
}

fn tone(milliseconds: usize) -> Vec<f32> {
    (0..ms(milliseconds))
        .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 300.0 / RATE as f32).sin() * 0.1)
        .collect()
}

/// Bursts of the given lengths separated by short (inter-word) pauses.
fn sentence(bursts: &[usize]) -> Vec<f32> {
    let mut samples = Vec::new();
    for (index, burst) in bursts.iter().enumerate() {
        if index > 0 {
            samples.extend(silence(200));
        }
        samples.extend(tone(*burst));
    }
    samples
}

const LONG: [usize; 9] = [300, 500, 300, 700, 300, 500, 300, 700, 500];

/// Two long sentences separated by 1.5 s pauses, then a short one that
/// stays in the tail. Words repeat within and across sentences.
fn fixture() -> Vec<f32> {
    let mut samples = silence(400);
    samples.extend(sentence(&LONG));
    samples.extend(silence(1_500));
    samples.extend(sentence(&LONG));
    samples.extend(silence(1_500));
    samples.extend(sentence(&[700, 300, 300]));
    samples.extend(silence(300));
    samples
}

/// Two long sentences, then a long pause, optionally a sound of `blip`
/// milliseconds (too short for the VAD) and another pause: the tail at stop
/// holds no voice.
fn ends_quiet(blip: Option<usize>) -> Vec<f32> {
    let mut samples = silence(400);
    samples.extend(sentence(&LONG));
    samples.extend(silence(1_500));
    samples.extend(sentence(&LONG));
    samples.extend(silence(2_500));
    if let Some(blip) = blip {
        samples.extend(tone(blip));
        samples.extend(silence(1_500));
    }
    samples
}

fn fixture_seconds() -> f64 {
    fixture().len() as f64 / f64::from(RATE)
}

fn config(directory: &Path) -> CaptureConfig {
    CaptureConfig {
        artifact_directory: Some(directory.to_owned()),
        ..CaptureConfig::default()
    }
}

/// A recording that grows by 150 ms per snapshot until the signal ends.
struct Tape {
    signal: Vec<f32>,
    config: CaptureConfig,
    snapshots: Arc<AtomicUsize>,
    supports_tail: bool,
    tail: Arc<Mutex<Option<FinalTail>>>,
}

impl Tape {
    const STEP: usize = 3_600;

    fn new(signal: Vec<f32>, directory: &Path) -> Self {
        Self {
            signal,
            config: config(directory),
            snapshots: Arc::default(),
            supports_tail: true,
            tail: Arc::default(),
        }
    }
}

impl AudioCapture for Tape {
    fn snapshot(&self, since: Duration) -> Result<AudioSnapshot, AudioError> {
        let length = (self.snapshots.fetch_add(1, Ordering::SeqCst) + 1) * Self::STEP;
        if length > self.signal.len() + Self::STEP {
            return Err(AudioError::NotCapturing);
        }
        let recorded = &self.signal[..length.min(self.signal.len())];
        let start = ((since.as_secs_f64() * f64::from(RATE)).round() as usize).min(recorded.len());
        Ok(AudioSnapshot::for_test(
            recorded[start..].to_vec(),
            self.config.clone(),
        ))
    }
    fn retain_final_tail(&mut self, tail: FinalTail) -> bool {
        if self.supports_tail {
            *self.tail.lock().unwrap() = Some(tail);
        }
        self.supports_tail
    }
    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn arm(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn disarm(&mut self) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn start(&mut self, _: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn stop(&mut self) -> AudioFuture<'_, audio::AudioArtifact> {
        panic!("the live loop never stops capture")
    }
    fn cancel(&mut self) -> AudioFuture<'_, ()> {
        panic!("the live loop never cancels capture")
    }
    fn state(&self) -> CaptureState {
        CaptureState::Capturing
    }
    fn level(&self) -> LevelMeter {
        LevelMeter::default()
    }
}

fn read_wav(path: &Path) -> (Vec<f32>, u32) {
    let bytes = fs::read(path).unwrap();
    let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
    let samples = bytes[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| f32::from(i16::from_le_bytes(*b)) / f32::from(i16::MAX))
        .collect();
    (samples, rate)
}

/// A deterministic stand-in for speech recognition: every voiced burst is a
/// word named after its length in tenths of a second, with its timing.
fn recognize_bursts(path: &Path) -> Transcript {
    let (samples, rate) = read_wav(path);
    let frame = (rate / 50) as usize;
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut run: Option<usize> = None;
    let close = |start: usize, end: usize, words: &mut Vec<(usize, usize)>| {
        if (end - start) * 1_000 / rate as usize >= 100 {
            words.push((start, end));
        }
    };
    for (index, values) in samples.chunks(frame).enumerate() {
        let rms = (values.iter().map(|v| v * v).sum::<f32>() / values.len() as f32).sqrt();
        match (rms >= 0.02, run) {
            (true, None) => run = Some(index * frame),
            (false, Some(start)) => {
                close(start, index * frame, &mut words);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(start) = run {
        close(start, samples.len(), &mut words);
    }
    let seconds = |at: usize| at as f64 / f64::from(rate);
    let segments: Vec<Segment> = words
        .iter()
        .map(|(start, end)| Segment {
            start: seconds(*start),
            end: seconds(*end),
            speaker: None,
            text: format!(" w{}", ((end - start) * 1_000 / rate as usize + 50) / 100),
        })
        .collect();
    Transcript {
        text: segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<String>()
            .trim()
            .to_owned(),
        segments,
        model: "bursts".into(),
        duration_ms: 1,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Call {
    preview: bool,
    seconds: f64,
    prompt: Option<String>,
    locale: Option<String>,
}

type ErrorFactory = fn() -> AsrError;

/// Recognizes bursts and records every request.
struct Bursts {
    residency: watch::Sender<ModelResidency>,
    tracks_residency: bool,
    calls: Mutex<Vec<Call>>,
    /// Final requests (1-based) that fail, and how.
    failures: Vec<(usize, ErrorFactory)>,
    /// Final requests wait here for cancellation when set.
    hold: Option<Arc<Notify>>,
}

impl Bursts {
    fn new() -> Self {
        Self {
            residency: watch::channel(ModelResidency::Resident(1)).0,
            tracks_residency: true,
            calls: Mutex::default(),
            failures: Vec::new(),
            hold: None,
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn finals(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| !call.preview)
            .collect()
    }

    fn record(
        &self,
        path: &Path,
        preview: bool,
        prompt: Option<&str>,
        locale: Option<&str>,
    ) -> usize {
        let (samples, rate) = read_wav(path);
        let mut calls = self.calls.lock().unwrap();
        calls.push(Call {
            preview,
            seconds: samples.len() as f64 / f64::from(rate),
            prompt: prompt.map(str::to_owned),
            locale: locale.map(str::to_owned),
        });
        calls.iter().filter(|call| !call.preview).count()
    }
}

#[async_trait::async_trait]
impl Transcriber for Bursts {
    async fn load(&self, _: &str) -> Result<(), AsrError> {
        Ok(())
    }
    async fn transcribe(
        &self,
        path: &Path,
        prompt: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        self.transcribe_with_locale(path, prompt, None, cancel)
            .await
    }
    async fn transcribe_with_locale(
        &self,
        path: &Path,
        prompt: Option<&str>,
        locale: Option<&str>,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        let number = self.record(path, false, prompt, locale);
        if let Some(hold) = &self.hold {
            hold.notify_one();
            let _ = cancel.wait_for(|cancelled| *cancelled).await;
            return Err(AsrError::Cancelled);
        }
        if let Some((_, error)) = self.failures.iter().find(|(at, _)| *at == number) {
            return Err(error());
        }
        Ok(recognize_bursts(path))
    }
    async fn transcribe_preview(
        &self,
        path: &Path,
        prompt: Option<&str>,
        locale: Option<&str>,
        _: crate::asr::PreviewDecoding,
        _: watch::Receiver<bool>,
    ) -> Result<Transcript, AsrError> {
        self.record(path, true, prompt, locale);
        Ok(recognize_bursts(path))
    }
    async fn shutdown(&self) -> Result<(), AsrError> {
        Ok(())
    }
    async fn reconfigure(&self, _: WorkerCommand) {}
    fn model_residency(&self) -> Option<watch::Receiver<ModelResidency>> {
        self.tracks_residency.then(|| self.residency.subscribe())
    }
}

fn settings() -> Settings {
    Settings {
        incremental_final_asr: true,
        speech_locale: Some("ja-JP".into()),
        ..Settings::default()
    }
}

const PROMPT: Option<&str> = Some("Tauri\nWhisper");

/// Records `tape` through the live loop with incremental recognition and
/// seals the tail as a stop would. Returns the take and the full WAV.
async fn record(
    tape: Tape,
    transcriber: &dyn Transcriber,
    settings: &Settings,
    preview: bool,
) -> (Take, audio::AudioArtifact) {
    let signal = tape.signal.clone();
    let config = tape.config.clone();
    let tail = Arc::clone(&tape.tail);
    let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> = tokio::sync::Mutex::new(Box::new(tape));
    let mut take = Take::plan(settings)
        .unwrap()
        .begin(PROMPT, settings.speech_locale.as_deref());
    let (_cancel, cancel) = watch::channel(false);
    let (_stop, stopped) = watch::channel(false);
    tokio::time::timeout(
        Duration::from_secs(60),
        super::super::run_loop(
            &audio,
            transcriber,
            PROMPT,
            settings.speech_locale.as_deref(),
            crate::asr::PreviewDecoding::Standard,
            cancel,
            stopped,
            Duration::from_millis(1),
            super::super::Work {
                preview,
                incremental: take.session_mut(),
            },
            |_| {},
        ),
    )
    .await
    .unwrap()
    .unwrap();
    if let Some(tail) = tail.lock().unwrap().as_ref() {
        tail.seal(&signal, RATE, &config);
    }
    let full = AudioSnapshot::for_test(signal, config)
        .into_artifact()
        .unwrap();
    (take, full)
}

fn ranges(take: &Take) -> Vec<(usize, usize)> {
    match take {
        Take::Recorded(session) => session
            .ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect(),
        _ => vec![],
    }
}

async fn stop(
    transcriber: &dyn Transcriber,
    take: Take,
    settings: &Settings,
    full: &audio::AudioArtifact,
    prompt: Option<&str>,
) -> (Transcript, Option<Outcome>) {
    let (_cancel, cancel) = watch::channel(false);
    let mut outcome = None;
    let transcript = transcribe_final(
        transcriber,
        Some(take),
        settings,
        &full.path,
        prompt,
        cancel,
        |value| outcome = Some(value),
    )
    .await
    .unwrap();
    (transcript, outcome)
}

fn fallback(outcome: Option<Outcome>) -> Option<Fallback> {
    outcome
        .expect("an enabled take records its outcome")
        .fallback
}

#[test]
fn boundaries_need_a_long_pause_after_enough_audio_and_never_cut_speech() {
    let find = |samples: &[f32]| find_boundary(samples, RATE, 0.01, Duration::from_millis(100));
    // A long pause after 6 s of speech: the cut is in its middle.
    let mut samples = tone(6_000);
    samples.extend(silence(1_200));
    samples.extend(tone(2_000));
    let cut = find(&samples).unwrap();
    assert!(cut.abs_diff(ms(6_600)) <= ms(20), "{cut}");
    // A pause still running at the end is cut half a pause after the voice.
    assert!(find(&samples[..ms(7_100)]).unwrap().abs_diff(ms(6_550)) <= ms(20));
    assert_eq!(find(&samples[..ms(6_900)]), None, "pause too short so far");
    // Inter-word pauses, however many, never qualify.
    assert_eq!(find(&sentence(&[700; 40])), None);
    let mut short_pause = tone(6_000);
    short_pause.extend(silence(900));
    short_pause.extend(tone(1_000));
    assert_eq!(find(&short_pause), None);
    // Too little audio before the pause: no range yet.
    let mut early = tone(3_000);
    early.extend(silence(1_500));
    early.extend(tone(3_000));
    assert_eq!(find(&early), None);
    // Long continuous speech is never cut.
    assert_eq!(find(&tone(40_000)), None);
    // The latest cut within 28 s wins; past that, the earliest.
    let mut many = Vec::new();
    for _ in 0..5 {
        many.extend(tone(6_000));
        many.extend(silence(1_200));
    }
    let latest = find(&many).unwrap();
    assert!(latest <= ms(28_000) && latest > ms(20_000), "{latest}");
    let mut late = tone(30_000);
    late.extend(silence(1_200));
    late.extend(tone(5_000));
    late.extend(silence(1_200));
    assert!(find(&late).unwrap().abs_diff(ms(30_600)) <= ms(20));
    // A silent stretch is a range of its own once long enough.
    assert!(find(&silence(10_400)).is_some());
    assert_eq!(find(&silence(9_000)), None);
}

#[test]
fn joining_concatenates_by_audio_order_and_never_deduplicates() {
    let part = |text: &str, model: &str| Transcript {
        text: text.into(),
        segments: vec![Segment {
            start: 0.5,
            end: 1.0,
            speaker: None,
            text: text.into(),
        }],
        model: model.into(),
        duration_ms: 10,
    };
    let joined = join([
        (0.0, part(" the the", "m")),
        (7.5, part(" the", "m")),
        (12.0, part("です。", "m")),
    ])
    .unwrap();
    assert_eq!(joined.text, "the the theです。");
    let starts: Vec<f64> = joined.segments.iter().map(|s| s.start).collect();
    assert_eq!(starts, vec![0.5, 8.0, 12.5]);
    assert_eq!(joined.duration_ms, 30);
    assert_eq!(joined.model, "m");
    assert_eq!(
        join([(0.0, part("a", "m")), (5.0, part("b", "other"))]),
        Err(Fallback::ModelChanged)
    );
    assert_eq!(join([]), Err(Fallback::EmptyResult));
}

#[test]
fn only_an_enabled_local_backend_plans_incremental_recognition() {
    assert!(Take::plan(&Settings::default()).is_none());
    assert!(matches!(Take::plan(&settings()), Some(Take::Planned(_))));
    let cloud = Settings {
        asr_backend: "openai-compatible".into(),
        ..settings()
    };
    assert!(matches!(
        Take::plan(&cloud),
        Some(Take::Unavailable(Fallback::CloudBackend))
    ));
    for backend in ["vibevoice", "mock"] {
        let local = Settings {
            asr_backend: backend.into(),
            ..settings()
        };
        assert!(matches!(Take::plan(&local), Some(Take::Planned(_))));
    }
}

/// Offline replay: the join of ranges recognized while recording and the
/// tail recognized at stop equals recognizing the whole recording, and stop
/// only recognizes the tail.
#[tokio::test]
async fn replayed_incremental_join_matches_whole_recording_recognition() {
    let directory = tempfile::tempdir().unwrap();
    let transcriber = Bursts::new();
    let settings = settings();
    let (take, full) = record(
        Tape::new(fixture(), directory.path()),
        &transcriber,
        &settings,
        false,
    )
    .await;
    let ranges = ranges(&take);
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    assert_eq!(transcriber.finals().len(), 2);
    let (joined, outcome) = stop(&transcriber, take, &settings, &full, PROMPT).await;
    assert_eq!(fallback(outcome), None);
    let whole = recognize_bursts(&full.path);
    assert_eq!(joined.text, whole.text);
    assert_eq!(whole.text.split(' ').count(), 21);
    assert_eq!(joined.segments.len(), whole.segments.len());
    for (joined, whole) in joined.segments.iter().zip(&whole.segments) {
        assert!(
            (joined.start - whole.start).abs() < 0.05,
            "{joined:?} {whole:?}"
        );
        assert_eq!(joined.text, whole.text);
    }
    // Stop sent one request for the tail only, as the final request.
    let finals = transcriber.finals();
    assert_eq!(finals.len(), 3);
    let total = fixture_seconds();
    assert!(
        finals[2].seconds < total / 4.0,
        "{} of {total}",
        finals[2].seconds
    );
    let covered: f64 = finals.iter().map(|call| call.seconds).sum();
    assert!((covered - total).abs() < 0.01, "{covered} of {total}");
    for call in finals {
        assert_eq!(call.prompt.as_deref(), PROMPT);
        assert_eq!(call.locale.as_deref(), Some("ja-JP"));
    }
    // No range or tail WAV is left behind.
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn preview_and_range_decodes_take_turns_in_the_same_loop() {
    let directory = tempfile::tempdir().unwrap();
    let transcriber = Bursts::new();
    let (take, _full) = record(
        Tape::new(fixture(), directory.path()),
        &transcriber,
        &settings(),
        true,
    )
    .await;
    assert_eq!(ranges(&take).len(), 2);
    let calls = transcriber.calls();
    assert!(calls.iter().filter(|call| call.preview).count() >= 3);
    assert_eq!(calls.iter().filter(|call| !call.preview).count(), 2);
}

fn session() -> Session {
    match Take::plan(&settings())
        .unwrap()
        .begin(PROMPT, Some("ja-JP"))
    {
        Take::Recorded(session) => *session,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn a_decode_yields_the_next_turn_to_the_preview_and_stop_starts_no_range() {
    let directory = tempfile::tempdir().unwrap();
    let tape = Tape::new(fixture(), directory.path());
    let snapshots = Arc::clone(&tape.snapshots);
    let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> = tokio::sync::Mutex::new(Box::new(tape));
    let transcriber = Bursts::new();
    let mut session = session();
    let (_cancel, cancel) = watch::channel(false);
    let (stop, stopped) = watch::channel(false);
    let mut turn = Turn::Idle;
    while turn != Turn::Used {
        turn = session
            .step(&audio, &transcriber, &cancel, &stopped, true)
            .await;
    }
    let before = snapshots.load(Ordering::SeqCst);
    assert_eq!(
        session
            .step(&audio, &transcriber, &cancel, &stopped, true)
            .await,
        Turn::Idle
    );
    assert_eq!(
        snapshots.load(Ordering::SeqCst),
        before,
        "the preview's turn"
    );
    // Once stop is signalled, no further range starts.
    stop.send_replace(true);
    for _ in 0..200 {
        assert_eq!(
            session
                .step(&audio, &transcriber, &cancel, &stopped, true)
                .await,
            Turn::Stop
        );
    }
    assert_eq!(transcriber.finals().len(), 1);
    assert_eq!(snapshots.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn cancelling_the_take_cancels_a_range_decode_in_flight() {
    let directory = tempfile::tempdir().unwrap();
    let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
        tokio::sync::Mutex::new(Box::new(Tape::new(fixture(), directory.path())));
    let hold = Arc::new(Notify::new());
    let transcriber = Bursts {
        hold: Some(Arc::clone(&hold)),
        ..Bursts::new()
    };
    let mut session = session();
    let (cancel_sender, cancel) = watch::channel(false);
    let (_stop, stopped) = watch::channel(false);
    let steps = async {
        loop {
            let turn = session
                .step(&audio, &transcriber, &cancel, &stopped, false)
                .await;
            if turn != Turn::Idle {
                return turn;
            }
        }
    };
    let canceller = async {
        hold.notified().await;
        cancel_sender.send_replace(true);
    };
    let (turn, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(steps, canceller)
    })
    .await
    .unwrap();
    assert_eq!(turn, Turn::Stop);
    assert_eq!(session.failure, Some(Fallback::RangeCancelled));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn off_sends_exactly_the_whole_recording_request_and_records_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let full = AudioSnapshot::for_test(fixture(), config(directory.path()))
        .into_artifact()
        .unwrap();
    let transcriber = Bursts::new();
    let (_cancel, cancel) = watch::channel(false);
    let mut recorded = false;
    let settings = Settings::default();
    let transcript = transcribe_final(
        &transcriber,
        Take::plan(&settings),
        &settings,
        &full.path,
        PROMPT,
        cancel,
        |_| recorded = true,
    )
    .await
    .unwrap();
    assert!(!recorded);
    assert_eq!(transcript, recognize_bursts(&full.path));
    assert_eq!(
        transcriber.calls(),
        vec![Call {
            preview: false,
            seconds: fixture_seconds(),
            prompt: PROMPT.map(str::to_owned),
            locale: None,
        }]
    );
}

fn tape(directory: &Path) -> Tape {
    Tape::new(fixture(), directory)
}

/// Records the fixture, applies `change` before stop, and expects a
/// whole-recording fallback for `reason`.
async fn expect_fallback(
    reason: Fallback,
    transcriber: Bursts,
    tape: impl FnOnce(&Path) -> Tape,
    change: impl FnOnce(&mut Settings, &mut Option<&'static str>, &Bursts, &mut Take),
) {
    let directory = tempfile::tempdir().unwrap();
    let mut settings = settings();
    let (mut take, full) = record(tape(directory.path()), &transcriber, &settings, false).await;
    let mut prompt = PROMPT;
    change(&mut settings, &mut prompt, &transcriber, &mut take);
    let before = transcriber.finals().len();
    let (transcript, outcome) = stop(&transcriber, take, &settings, &full, prompt).await;
    assert_eq!(fallback(outcome), Some(reason), "{reason:?}");
    assert_eq!(transcript, recognize_bursts(&full.path));
    // The fallback is the whole-recording request, sent last.
    let finals = transcriber.finals();
    assert!(finals.len() > before);
    let last = finals.last().unwrap();
    let total = full.sample_count as f64 / f64::from(full.sample_rate);
    assert!((last.seconds - total).abs() < 0.01);
    assert_eq!(last.prompt.as_deref(), prompt);
}

fn no_change(_: &mut Settings, _: &mut Option<&'static str>, _: &Bursts, _: &mut Take) {}

fn recorded(take: &mut Take) -> &mut Session {
    take.session_mut().expect("a recorded take")
}

#[tokio::test]
async fn every_identity_change_falls_back_to_whole_recording_recognition() {
    expect_fallback(
        Fallback::PromptChanged,
        Bursts::new(),
        tape,
        |_, prompt, _, _| *prompt = Some("Tauri\nWhisper\nnew term"),
    )
    .await;
    expect_fallback(
        Fallback::SettingsChanged,
        Bursts::new(),
        tape,
        |settings, _, _, _| settings.speech_locale = Some("en-US".into()),
    )
    .await;
    expect_fallback(
        Fallback::SettingsChanged,
        Bursts::new(),
        tape,
        |settings, _, _, _| settings.model_quantization = "8bit".into(),
    )
    .await;
    expect_fallback(
        Fallback::SettingsChanged,
        Bursts::new(),
        tape,
        |settings, _, _, _| settings.incremental_final_asr = false,
    )
    .await;
    expect_fallback(
        Fallback::CloudBackend,
        Bursts::new(),
        tape,
        |settings, _, _, _| settings.asr_backend = "openai-compatible".into(),
    )
    .await;
    expect_fallback(
        Fallback::ModelChanged,
        Bursts::new(),
        tape,
        |_, _, transcriber, _| {
            transcriber
                .residency
                .send_replace(ModelResidency::Resident(2));
        },
    )
    .await;
    expect_fallback(
        Fallback::ModelChanged,
        Bursts::new(),
        tape,
        |_, _, transcriber, _| {
            transcriber.residency.send_replace(ModelResidency::Absent);
        },
    )
    .await;
    let untracked = Bursts {
        tracks_residency: false,
        ..Bursts::new()
    };
    expect_fallback(Fallback::ModelUnverified, untracked, tape, no_change).await;
}

#[tokio::test]
async fn failed_ranges_tails_and_coverage_fall_back() {
    let failing = |at: usize, error: ErrorFactory| Bursts {
        failures: vec![(at, error)],
        ..Bursts::new()
    };
    let crashed: ErrorFactory = || AsrError::Crashed;
    let cancelled: ErrorFactory = || AsrError::Cancelled;
    expect_fallback(Fallback::RangeFailed, failing(1, crashed), tape, no_change).await;
    expect_fallback(
        Fallback::RangeCancelled,
        failing(2, cancelled),
        tape,
        no_change,
    )
    .await;
    expect_fallback(Fallback::TailFailed, failing(3, crashed), tape, no_change).await;
    let unsupported = |directory: &Path| Tape {
        supports_tail: false,
        ..tape(directory)
    };
    expect_fallback(
        Fallback::MissingCoverage,
        Bursts::new(),
        unsupported,
        no_change,
    )
    .await;
    // A tail that was never handed over (the recording did not stop cleanly).
    expect_fallback(
        Fallback::MissingCoverage,
        Bursts::new(),
        tape,
        |_, _, _, take| {
            recorded(take).tail.take();
        },
    )
    .await;
    // A range missing from the middle.
    expect_fallback(
        Fallback::MissingCoverage,
        Bursts::new(),
        tape,
        |_, _, _, take| {
            recorded(take).ranges.remove(0);
        },
    )
    .await;
    // The tail prepared with another configuration.
    expect_fallback(
        Fallback::PreparationChanged,
        Bursts::new(),
        tape,
        |_, _, _, take| {
            recorded(take).config.as_mut().unwrap().enhancement.gain = 2.0;
        },
    )
    .await;
    // The tail cannot be written.
    expect_fallback(
        Fallback::PreparationFailed,
        Bursts::new(),
        tape,
        |_, _, _, take| {
            let session = recorded(take);
            let sealed = session.tail.take().unwrap();
            let mut config = sealed.audio.unwrap().config().clone();
            config.artifact_directory = Some(PathBuf::from("missing-incremental-test-dir/x"));
            let tail = FinalTail::default();
            assert!(tail.advance(sealed.from));
            tail.seal(&fixture(), RATE, &config);
            session.tail = tail;
        },
    )
    .await;
}

#[tokio::test]
async fn short_recordings_empty_joins_and_cloud_takes_fall_back() {
    let short = |directory: &Path| {
        let mut signal = silence(300);
        signal.extend(sentence(&[300, 500]));
        signal.extend(silence(300));
        Tape::new(signal, directory)
    };
    let directory = tempfile::tempdir().unwrap();
    let transcriber = Bursts::new();
    let settings = settings();
    let (take, full) = record(short(directory.path()), &transcriber, &settings, false).await;
    let (_, outcome) = stop(&transcriber, take, &settings, &full, PROMPT).await;
    assert_eq!(fallback(outcome), Some(Fallback::NoRanges));

    // A tail without voice needs no decode and adds no text.
    expect_fallback(
        Fallback::EmptyResult,
        Bursts::new(),
        |directory| Tape::new(ends_quiet(None), directory),
        |_, _, _, take| {
            for range in &mut recorded(take).ranges {
                if let Some(transcript) = range.transcript.as_mut() {
                    transcript.text.clear();
                    transcript.segments.clear();
                }
            }
        },
    )
    .await;

    let cloud = Settings {
        asr_backend: "openai-compatible".into(),
        ..settings.clone()
    };
    let full = AudioSnapshot::for_test(fixture(), config(directory.path()))
        .into_artifact()
        .unwrap();
    let transcriber = Bursts::new();
    let (_, outcome) = stop(
        &transcriber,
        Take::plan(&cloud).unwrap(),
        &cloud,
        &full,
        PROMPT,
    )
    .await;
    assert_eq!(fallback(outcome), Some(Fallback::CloudBackend));
    assert_eq!(transcriber.finals().len(), 1);
}

/// A tail without sound in the whole recording joins as silence: stop
/// sends no request at all.
#[tokio::test]
async fn a_quiet_tail_is_silence_in_the_whole_recording_too() {
    let directory = tempfile::tempdir().unwrap();
    let transcriber = Bursts::new();
    let settings = settings();
    let (take, full) = record(
        Tape::new(ends_quiet(None), directory.path()),
        &transcriber,
        &settings,
        false,
    )
    .await;
    assert_eq!(ranges(&take).len(), 2);
    let before = transcriber.finals().len();
    let (joined, outcome) = stop(&transcriber, take, &settings, &full, PROMPT).await;
    assert_eq!(fallback(outcome), None);
    assert_eq!(transcriber.finals().len(), before);
    assert_eq!(joined.text, recognize_bursts(&full.path).text);
}

/// A tail whose only sound is too short for the VAD on its own is not
/// dropped: the whole recording reaches the threshold there, so it is
/// recognized instead.
#[tokio::test]
async fn a_tail_with_sound_the_whole_recording_keeps_falls_back() {
    expect_fallback(
        Fallback::VoiceMismatch,
        Bursts::new(),
        |directory| Tape::new(ends_quiet(Some(40)), directory),
        no_change,
    )
    .await;
}

#[tokio::test]
async fn a_cancelled_stop_records_no_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let transcriber = Bursts::new();
    let settings = settings();
    let (take, full) = record(tape(directory.path()), &transcriber, &settings, false).await;
    let (cancel_sender, cancel) = watch::channel(false);
    cancel_sender.send_replace(true);
    let mut recorded = false;
    let result = transcribe_final(
        &transcriber,
        Some(take),
        &settings,
        &full.path,
        PROMPT,
        cancel,
        |_| recorded = true,
    )
    .await;
    assert!(matches!(result, Err(AsrError::Cancelled)));
    assert!(!recorded);
}

#[test]
fn fallback_codes_are_fixed_and_distinct() {
    let all = [
        Fallback::CloudBackend,
        Fallback::NoRanges,
        Fallback::SettingsChanged,
        Fallback::PromptChanged,
        Fallback::ModelChanged,
        Fallback::ModelUnverified,
        Fallback::RangeFailed,
        Fallback::RangeCancelled,
        Fallback::MissingCoverage,
        Fallback::PreparationChanged,
        Fallback::PreparationFailed,
        Fallback::VoiceMismatch,
        Fallback::TailFailed,
        Fallback::EmptyResult,
    ];
    let codes: std::collections::HashSet<_> = all.iter().map(|reason| reason.code()).collect();
    assert_eq!(codes.len(), all.len());
    assert!(codes
        .iter()
        .all(|code| code.chars().all(|c| c.is_ascii_lowercase() || c == '_')));
}

#[test]
fn outcomes_are_recorded_as_content_free_metrics() {
    let storage = crate::storage::Storage::in_memory().unwrap();
    let metrics = storage.metrics_for(crate::state::PipelineMode::Dictate, None);
    Outcome {
        fallback: None,
        elapsed: Duration::from_millis(42),
    }
    .record(metrics, "faster-whisper");
    Outcome {
        fallback: Some(Fallback::PromptChanged),
        elapsed: Duration::from_millis(7),
    }
    .record(metrics, "faster-whisper");
    let outcomes: Vec<_> = storage
        .diagnostics()
        .unwrap()
        .outcomes
        .into_iter()
        .filter(|outcome| outcome.event_type == METRIC_EVENT)
        .map(|outcome| (outcome.success, outcome.code, outcome.count))
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (false, Some("prompt_changed".to_owned()), 1),
            (true, None, 1)
        ]
    );
    // Only a used join is timed; a fallback's time is in the `asr` event.
    let timed: Vec<_> = storage
        .diagnostics()
        .unwrap()
        .summaries
        .into_iter()
        .filter(|summary| summary.event_type == METRIC_EVENT)
        .map(|summary| (summary.count, summary.timed_count, summary.p50_ms))
        .collect();
    assert_eq!(timed, vec![(2, 1, Some(42))]);
}

/// Opt-in replay of reference speech through the installed faster-whisper
/// model: incremental join versus whole-recording recognition. Writes a
/// report next to the input; whether the texts are equally accurate is for
/// a person to judge. Run with
/// `ASR_INCREMENTAL_BENCH_DIR=<dir with speech.f32> cargo test --lib
/// incremental_replay_on_reference_audio -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "requires ASR_INCREMENTAL_BENCH_DIR with speech.f32 (24 kHz mono f32le); uses the installed faster-whisper model"]
async fn incremental_replay_on_reference_audio() {
    use crate::asr::JsonlTranscriber;
    use crate::{worker_command_for_settings, ASR_MODEL_LOAD_TIMEOUT, ASR_REQUEST_TIMEOUT};
    let directory = PathBuf::from(
        std::env::var_os("ASR_INCREMENTAL_BENCH_DIR").expect("ASR_INCREMENTAL_BENCH_DIR"),
    );
    let bytes = fs::read(directory.join("speech.f32")).unwrap();
    let signal: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    let settings = Settings {
        incremental_final_asr: true,
        ..Settings::default()
    };
    let transcriber = JsonlTranscriber::new(
        worker_command_for_settings(&settings),
        ASR_REQUEST_TIMEOUT,
        ASR_MODEL_LOAD_TIMEOUT,
    );
    transcriber
        .load(&settings.model_quantization)
        .await
        .unwrap();
    let work = tempfile::tempdir_in(&directory).unwrap();
    let recording = Instant::now();
    let (take, full) = record(
        Tape::new(signal.clone(), work.path()),
        &transcriber,
        &settings,
        false,
    )
    .await;
    let recording_seconds = recording.elapsed().as_secs_f64();
    let range_count = ranges(&take).len();
    let incremental_started = Instant::now();
    let (joined, outcome) = stop(&transcriber, take, &settings, &full, PROMPT).await;
    let incremental_seconds = incremental_started.elapsed().as_secs_f64();
    let (_cancel, cancel) = watch::channel(false);
    let whole_started = Instant::now();
    let whole = transcriber
        .transcribe_with_locale(&full.path, PROMPT, None, cancel)
        .await
        .unwrap();
    let whole_seconds = whole_started.elapsed().as_secs_f64();
    let report = serde_json::json!({
        "audio_seconds": signal.len() as f64 / f64::from(RATE),
        "ranges": range_count,
        "fallback": outcome.and_then(|outcome| outcome.fallback).map(Fallback::code),
        "replay_seconds_with_range_decodes": recording_seconds,
        "stop_seconds_incremental": incremental_seconds,
        "stop_seconds_whole": whole_seconds,
        "identical_text": joined.text == whole.text,
        "incremental_text": joined.text,
        "whole_text": whole.text,
    });
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let path = directory.join(format!("incremental-report-{stamp}.json"));
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    serde_json::to_writer_pretty(output, &report).unwrap();
    println!("report {}", path.display());
    transcriber.shutdown().await.unwrap();
}

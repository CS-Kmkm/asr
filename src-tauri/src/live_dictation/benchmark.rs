//! Opt-in real-time replay through the actual warmed worker and segmentation
//! policy. Input is synthetic/reference PCM, never microphone or history data.
use super::*;
use crate::audio::{AudioSnapshot, TARGET_SAMPLE_RATE};
use serde_json::json;

#[tokio::test]
#[ignore = "requires ASR_LIVE_BENCH_DIR with speech.f32; uses the installed faster-whisper model"]
async fn compare_segmentation_on_reference_audio() {
    let directory =
        PathBuf::from(std::env::var_os("ASR_LIVE_BENCH_DIR").expect("ASR_LIVE_BENCH_DIR"));
    let bytes = fs::read(directory.join("speech.f32")).unwrap();
    assert_eq!(bytes.len() % 4, 0);
    let samples: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let config = CaptureConfig {
        artifact_directory: Some(directory.clone()),
        ..CaptureConfig::default()
    };
    // Matches the user's selected backend and quantization for this comparison.
    let settings = Settings {
        asr_backend: "faster-whisper".into(),
        model_quantization: "bf16".into(),
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
    let warm = AudioSnapshot::for_test(
        samples[..samples.len().min(3 * TARGET_SAMPLE_RATE as usize)].to_vec(),
        config.clone(),
    )
    .into_artifact()
    .unwrap();
    let warm_cleanup = TempArtifact::new(warm.path.clone(), false);
    let (_cancel, cancel) = watch::channel(false);
    transcriber
        .transcribe(&warm.path, None, cancel.clone())
        .await
        .unwrap();
    drop(warm_cleanup);
    let mut reports = vec![];
    for (name, policy) in [
        ("cumulative_1500ms", None),
        ("utterance_650ms", Some(Segmentation::default())),
        (
            "utterance_450ms",
            Some({
                let mut policy = Segmentation::default();
                policy.silence = Duration::from_millis(450);
                policy
            }),
        ),
    ] {
        println!("replay_start {name}");
        reports.push(
            replay(
                name,
                policy,
                &samples,
                &config,
                &transcriber,
                cancel.clone(),
            )
            .await,
        );
        println!("replay_complete {name}");
    }
    let full = AudioSnapshot::for_test(samples.clone(), config)
        .into_artifact()
        .unwrap();
    let cleanup = TempArtifact::new(full.path.clone(), false);
    let started = Instant::now();
    let transcript = transcriber
        .transcribe(&full.path, None, cancel)
        .await
        .unwrap();
    let report = json!({"sample_rate": TARGET_SAMPLE_RATE, "audio_seconds": samples.len() as f64 / f64::from(TARGET_SAMPLE_RATE),
        "backend": transcript.model, "quantization": settings.model_quantization, "policies": reports,
        "whole_recording": {"text": transcript.text, "inference_seconds": started.elapsed().as_secs_f64()} });
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let path = directory.join(format!("report-{stamp}.json"));
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    serde_json::to_writer_pretty(output, &report).unwrap();
    println!("report {}", path.display());
    drop(cleanup);
    transcriber.shutdown().await.unwrap();
}

async fn replay(
    name: &str,
    mut policy: Option<Segmentation>,
    source: &[f32],
    config: &CaptureConfig,
    transcriber: &dyn Transcriber,
    cancel: watch::Receiver<bool>,
) -> serde_json::Value {
    let start = Instant::now();
    let duration = Duration::from_secs_f64(source.len() as f64 / f64::from(TARGET_SAMPLE_RATE));
    let mut cursor = Duration::ZERO;
    let mut completed = String::new();
    let mut displayed = String::new();
    let mut updates = vec![];
    let mut calls = vec![];
    let mut preprocessing = Duration::ZERO;
    loop {
        tokio::time::sleep(if policy.is_some() {
            LIVE_INTERVAL
        } else {
            Duration::from_millis(1500)
        })
        .await;
        let now = start.elapsed();
        if now >= duration {
            break;
        }
        let preprocess_started = Instant::now();
        let begin = (cursor.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)).round() as usize;
        let end = ((now.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize).min(source.len());
        let mut prepared =
            match AudioSnapshot::for_test(source[begin.min(end)..end].to_vec(), config.clone())
                .prepare()
            {
                Ok(value) => value,
                Err(AudioError::TooShort { .. }) => continue,
                Err(error) => panic!("preparation: {error}"),
            };
        let (count, endpoint, voice) = if let Some(policy) = &policy {
            match policy.plan(&prepared) {
                Decision::Wait => {
                    preprocessing += preprocess_started.elapsed();
                    continue;
                }
                Decision::Skip(silence) => {
                    preprocessing += preprocess_started.elapsed();
                    cursor += silence;
                    continue;
                }
                Decision::Decode {
                    samples,
                    endpoint,
                    voiced_until,
                } => (samples, endpoint, voiced_until),
            }
        } else {
            (prepared.samples.len(), false, prepared.duration())
        };
        prepared.samples.truncate(count);
        preprocessing += preprocess_started.elapsed();
        let window_duration = prepared.duration();
        let artifact = match prepared.into_artifact() {
            Ok(value) => value,
            Err(AudioError::NoVoiceDetected | AudioError::TooShort { .. }) => continue,
            Err(error) => panic!("artifact: {error}"),
        };
        let cleanup = TempArtifact::new(artifact.path.clone(), false);
        let infer_start = Instant::now();
        let result = transcriber
            .transcribe(&artifact.path, None, cancel.clone())
            .await
            .unwrap();
        let cost = infer_start.elapsed();
        drop(cleanup);
        let received = start.elapsed();
        calls.push(json!({"start_seconds": cursor.as_secs_f64(), "window_seconds": window_duration.as_secs_f64(),
            "inference_seconds": cost.as_secs_f64(), "received_seconds": received.as_secs_f64(), "endpoint": endpoint}));
        if received >= duration {
            break;
        }
        assert!(
            !result.text.trim().is_empty(),
            "empty live result for voiced reference input"
        );
        let text = append_utterance(&completed, &result.text);
        if text != displayed {
            updates.push(json!({"received_seconds": received.as_secs_f64(), "audio_end_seconds": (cursor + window_duration).as_secs_f64(), "text": text}));
            displayed = text.clone();
        }
        if let Some(policy) = &mut policy {
            policy.recognized(window_duration, voice, cost, endpoint);
        }
        if endpoint {
            completed = text;
            cursor += window_duration;
        }
    }
    json!({"policy": name, "calls": calls, "updates": updates, "last_draft": displayed,
        "preprocessing_seconds": preprocessing.as_secs_f64()})
}

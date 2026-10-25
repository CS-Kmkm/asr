#![cfg(test)]

use super::*;

#[test]
fn prepared_model_must_match_the_selected_model_and_quantization() {
    let settings = Settings::default();
    let mut current = ModelStatus {
        model_id: model_identity(&settings).0,
        quantization: settings.model_quantization.clone(),
        installed: true,
        state: "ready".into(),
        detail: String::new(),
    };
    assert!(is_prepared_model(&current, &settings));
    current.quantization = "8bit".into();
    assert!(!is_prepared_model(&current, &settings));
    current.quantization = settings.model_quantization.clone();
    current.state = "loading".into();
    assert!(!is_prepared_model(&current, &settings));
}

fn status(state: &str, detail: &str) -> ModelStatus {
    ModelStatus {
        model_id: Some("model".into()),
        quantization: "4bit".into(),
        installed: state == "ready",
        state: state.into(),
        detail: detail.into(),
    }
}

#[test]
fn a_slower_older_load_never_overwrites_a_newer_loads_status() {
    for (newer_outcome, older_outcome) in [
        ("ready", "ready"),
        ("ready", "error"),
        ("error", "ready"),
        ("error", "error"),
    ] {
        let slot = ModelStatusSlot::new(status("not_loaded", "d"));
        let older = slot.begin_load(status("loading", "d")).unwrap();
        let newer = slot.begin_load(status("loading", "d")).unwrap();

        // B finishes first, then A's outcome arrives late.
        assert!(slot.update(newer, status(newer_outcome, "B")).is_some());
        assert!(slot.update(older, status(older_outcome, "A")).is_none());
        let shown = slot.get().unwrap();
        assert_eq!(
            (shown.state.as_str(), shown.detail.as_str()),
            (newer_outcome, "B")
        );
    }
}

#[test]
fn an_older_load_finishing_first_is_replaced_when_the_newer_one_ends() {
    let slot = ModelStatusSlot::new(status("not_loaded", "d"));
    let older = slot.begin_load(status("loading", "d")).unwrap();
    let newer = slot.begin_load(status("loading", "d")).unwrap();

    // Identical "loading" statuses: A's cancel must not touch B's download.
    assert!(slot.update(older, status("not_loaded", "A")).is_none());
    assert_eq!(slot.get().unwrap().state, "loading");
    assert!(slot.update(older, status("ready", "A")).is_none());
    assert!(slot.update(newer, status("error", "B")).is_some());
    assert_eq!(slot.get().unwrap().detail, "B");
}

#[test]
fn a_settings_save_takes_the_status_from_every_load() {
    let slot = ModelStatusSlot::new(status("not_loaded", "d"));
    let load = slot.begin_load(status("loading", "d")).unwrap();
    slot.replace(status("not_loaded", "other model")).unwrap();

    assert!(slot.update(load, status("ready", "d")).is_none());
    assert_eq!(slot.get().unwrap().detail, "other model");
}

/// Loads run one at a time like the worker's: a load holds `worker`
/// until `release` opens.
struct QueuedTranscriber {
    worker: tokio::sync::Mutex<()>,
    release: tokio::sync::watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl Transcriber for QueuedTranscriber {
    async fn load(&self, _: &str) -> Result<(), asr::AsrError> {
        unreachable!("loads go through load_when_started")
    }

    async fn load_when_started(
        &self,
        _: &str,
        cancel: Option<tokio::sync::watch::Receiver<bool>>,
        started: &(dyn Fn() + Send + Sync),
    ) -> Result<(), asr::AsrError> {
        let _worker = match cancel {
            Some(mut cancel) => tokio::select! {
                worker = self.worker.lock() => worker,
                _ = cancel.wait_for(|cancelled| *cancelled) => {
                    return Err(asr::AsrError::Cancelled);
                }
            },
            None => self.worker.lock().await,
        };
        started();
        let mut release = self.release.clone();
        let _ = release.wait_for(|open| *open).await;
        Ok(())
    }

    async fn transcribe(
        &self,
        _: &Path,
        _: Option<&str>,
        _: tokio::sync::watch::Receiver<bool>,
    ) -> Result<asr::Transcript, asr::AsrError> {
        unreachable!("not used")
    }

    async fn shutdown(&self) -> Result<(), asr::AsrError> {
        Ok(())
    }

    async fn reconfigure(&self, _: asr::WorkerCommand) {}
}

fn spawn_load(
    slot: &Arc<ModelStatusSlot>,
    transcriber: &Arc<QueuedTranscriber>,
    cancel: Option<tokio::sync::watch::Receiver<bool>>,
) -> tokio::task::JoinHandle<(Result<(), String>, Option<u64>)> {
    let slot = slot.clone();
    let transcriber = transcriber.clone();
    tokio::spawn(async move {
        let loading = status("loading", "d");
        let ready = status("ready", "d Loaded with 4bit quantization.");
        load_and_publish_status(
            &slot,
            transcriber.as_ref(),
            "4bit",
            cancel,
            ModelLoadStatuses {
                loading: &loading,
                ready: &ready,
                detail: "d",
            },
            &|_| {},
        )
        .await
    })
}

#[tokio::test]
async fn a_load_cancelled_while_waiting_leaves_the_running_load_its_status() {
    // The startup download (A) is running when a recording start (B)
    // needs the model; Esc cancels B before it ever reaches the worker.
    let slot = Arc::new(ModelStatusSlot::new(status("not_loaded", "d")));
    let (release_tx, release) = tokio::sync::watch::channel(false);
    let transcriber = Arc::new(QueuedTranscriber {
        worker: tokio::sync::Mutex::new(()),
        release,
    });
    let startup = spawn_load(&slot, &transcriber, None);
    while slot.get().unwrap().state != "loading" {
        tokio::task::yield_now().await;
    }

    let (cancel_tx, cancel) = tokio::sync::watch::channel(false);
    let recording = spawn_load(&slot, &transcriber, Some(cancel));
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel_tx.send_replace(true);
    let (result, shown_by) = recording.await.unwrap();
    assert_eq!(result, Err(MODEL_LOAD_CANCELLED.to_string()));
    assert_eq!(shown_by, None);
    assert_eq!(slot.get().unwrap().state, "loading");

    release_tx.send_replace(true);
    let (result, shown_by) = startup.await.unwrap();
    assert_eq!(result, Ok(()));
    assert!(shown_by.is_some());
    assert_eq!(slot.get().unwrap().state, "ready");
}

#[tokio::test]
async fn a_load_that_reaches_the_worker_takes_the_status_over() {
    let slot = Arc::new(ModelStatusSlot::new(status("not_loaded", "d")));
    let (release_tx, release) = tokio::sync::watch::channel(true);
    let transcriber = Arc::new(QueuedTranscriber {
        worker: tokio::sync::Mutex::new(()),
        release,
    });
    let (result, first) = spawn_load(&slot, &transcriber, None).await.unwrap();
    assert_eq!(result, Ok(()));
    let (result, second) = spawn_load(&slot, &transcriber, None).await.unwrap();
    assert_eq!(result, Ok(()));
    assert!(first.is_some() && second.is_some() && first != second);
    // The first load no longer owns the status.
    assert!(slot
        .update(first.unwrap(), status("error", "late"))
        .is_none());
    drop(release_tx);
}

#[test]
fn quantization_save_takes_status_away_from_an_old_load() {
    let slot = ModelStatusSlot::new(status("not_loaded", "d"));
    let old = slot.begin_load(status("loading", "d")).unwrap();
    let mut changed = status("not_loaded", "new quantization");
    changed.quantization = "8bit".into();
    slot.replace(changed.clone()).unwrap();
    assert!(slot
        .update(old, status("ready", "old quantization"))
        .is_none());
    assert!(same_model_status(&slot.get().unwrap(), &changed));
}

#[test]
fn slots_do_not_share_ownership() {
    let first = ModelStatusSlot::new(status("not_loaded", "d"));
    let second = ModelStatusSlot::new(status("not_loaded", "d"));
    let load = first.begin_load(status("loading", "d")).unwrap();
    second.begin_load(status("loading", "d")).unwrap();
    second.begin_load(status("loading", "d")).unwrap();

    assert!(first.update(load, status("ready", "d")).is_some());
}

#[test]
fn residency_maps_to_the_status_of_the_loaded_model() {
    let ready = status("ready", "d Loaded with 4bit quantization.");
    assert!(same_model_status(
        &residency_status(asr::ModelResidency::Resident(3), &ready, "d"),
        &ready
    ));
    let restoring = residency_status(asr::ModelResidency::Restoring, &ready, "d");
    assert_eq!(
        (restoring.state.as_str(), restoring.installed),
        ("loading", false)
    );
    assert_eq!(restoring.detail, "d");
    let absent = residency_status(asr::ModelResidency::Absent, &ready, "d");
    assert_eq!(
        (absent.state.as_str(), absent.installed),
        ("not_loaded", false)
    );
    assert_eq!(absent.model_id, ready.model_id);
}

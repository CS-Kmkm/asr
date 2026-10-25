//! Setup exercises real ASR/insertion without retaining diagnostic utterances.
use super::{PipelineContext, RecordedTake};
use crate::{
    commands::{commit_dictation_output, report_clipboard_not_restored, CommittedOutput},
    emit_stage, emit_state,
    injection::{InsertResult, SystemTextInjector, TextInjector},
    live_dictation::LiveDraft,
    recording_overlay,
    state::{AppState, TerminalPhaseGuard},
    storage::Storage,
    types::{AppPhase, AppStage, OperationKind, RecordingResult},
    PipelineGuard, Services,
};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::watch;

pub(super) async fn finish_dictation_test(
    context: PipelineContext<'_>,
    take: RecordedTake,
) -> Result<RecordingResult, String> {
    let text = take.transcript.text;
    let duration_ms = take.artifact.duration.as_millis() as u64;
    let latency_ms = take.started.elapsed().as_millis() as u64;
    let app = context.app.clone();
    tokio::task::spawn_blocking(move || {
        let mut cleanup = take.artifact_cleanup;
        super::finish::report_temp_cleanup(&app, &mut cleanup);
    })
    .await
    .map_err(|_| "Setup test failed.".to_owned())?;
    context
        .services
        .lifecycle
        .commit_side_effect(context.operation_id, || Ok::<_, String>(()))
        .map_err(str::to_owned)??;
    recording_overlay::set_phase(context.app, &AppPhase::Completed);
    let snapshot = context.state.complete(
        text.clone(),
        "Dictation test completed without inserting text.".into(),
    );
    let _ = context.app.emit("app-state", snapshot);
    Ok(RecordingResult {
        text,
        insertion: "test_only".into(),
        duration_ms,
        latency_ms,
    })
}

async fn wait_for_switch(delay: Duration, mut cancel: watch::Receiver<bool>) -> bool {
    if *cancel.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        _ = async { loop { if *cancel.borrow() || cancel.changed().await.is_err() { return; } } } => false,
        _ = tokio::time::sleep(delay) => !*cancel.borrow(),
    }
}

fn insertion_message(result: InsertResult) -> &'static str {
    match result {
        InsertResult::ClipboardPaste => "Insertion test completed.",
        InsertResult::ClipboardOnly => {
            "The test text is on the clipboard; automatic insertion was skipped."
        }
        InsertResult::PasteUnverified => {
            "The test paste was sent. Check the target app before pasting again."
        }
    }
}

fn insertion_failure_message(error: &str) -> &'static str {
    if error == "Choose another app for the insertion test." {
        "Choose another app for the insertion test."
    } else {
        "Insertion test failed. Choose an editable field in another app and try again."
    }
}

pub(crate) async fn run_insertion(app: AppHandle, id: u64, cancel: watch::Receiver<bool>) {
    let services = app.state::<Services>();
    let state = app.state::<AppState>();
    let _owner = PipelineGuard {
        lifecycle: &services.lifecycle,
        id,
    };
    let _terminal = TerminalPhaseGuard::new(
        &state,
        &services.lifecycle,
        id,
        "Setup test cancelled.",
        |phase, message| emit_state(&app, &state, phase, message),
    );
    let result: Result<RecordingResult, String> = async {
        state.clear_result();
        state.set_operation(OperationKind::InsertionTest, id);
        recording_overlay::anchor_to_foreground();
        emit_stage(
            &app,
            &state,
            AppStage::Inserting,
            "Switch to another app within 3 seconds to test insertion.",
        );
        if !wait_for_switch(Duration::from_secs(3), cancel.clone()).await {
            return Err("Setup test cancelled.".into());
        }
        let started = Instant::now();
        let prepare_app = app.clone();
        let (mut draft, text) = tokio::task::spawn_blocking(move || {
            let settings = prepare_app
                .state::<Storage>()
                .get_settings()
                .map_err(crate::command_error)?;
            let target = SystemTextInjector::default()
                .capture_target()
                .map_err(crate::command_error)?;
            if target.process_id == std::process::id() {
                return Err("Choose another app for the insertion test.".to_owned());
            }
            let text = if settings.ui_language == "ja" {
                "音声入力の挿入テストです。"
            } else {
                "Local Voice Input insertion test."
            }
            .to_owned();
            let mut draft = LiveDraft::new(
                target,
                &settings,
                &settings.shortcuts.dictate[0],
                &settings.shortcuts.dictate,
                false,
                true,
            );
            let loss_app = prepare_app.clone();
            draft
                .injector
                .report_clipboard_loss_on_drop(move || report_clipboard_not_restored(&loss_app));
            Ok::<_, String>((draft, text))
        })
        .await
        .map_err(|_| "Setup test failed.".to_owned())??;
        draft.monitor.observe_cancellation(Some(cancel.clone()));
        let commit_app = app.clone();
        let result_text = text.clone();
        let runtime = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
            runtime.block_on(draft.settle_before_final(&cancel));
            let services = commit_app.state::<Services>();
            let result = commit_dictation_output(&services.lifecycle, id, || {
                draft.finish(&result_text, &cancel)
            });
            let clipboard_lost = draft.injector.take_clipboard_not_restored();
            if clipboard_lost {
                report_clipboard_not_restored(&commit_app);
            }
            match result {
                CommittedOutput::Ran(result) => result.map_err(crate::command_error),
                CommittedOutput::Cancelled => Err("Setup test cancelled.".into()),
                CommittedOutput::Refused(error) => Err(error.into()),
            }
        })
        .await
        .map_err(|_| "Setup test failed.".to_owned())??;
        recording_overlay::set_phase(&app, &AppPhase::Completed);
        let snapshot = state.complete(text.clone(), insertion_message(result).into());
        let _ = app.emit("app-state", snapshot);
        Ok(RecordingResult {
            text,
            insertion: match result {
                InsertResult::ClipboardPaste => "clipboard_paste",
                InsertResult::ClipboardOnly => "clipboard_only",
                InsertResult::PasteUnverified => "paste_unverified",
            }
            .into(),
            duration_ms: 0,
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
    .await;
    if let Err(error) = result {
        if !services.lifecycle.is_cancelled(id) {
            emit_state(
                &app,
                &state,
                AppPhase::Error,
                insertion_failure_message(&error),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn insertion_countdown_cancels_without_waiting_for_the_deadline() {
        let (sender, cancel) = watch::channel(false);
        let waiting = wait_for_switch(Duration::from_secs(30), cancel);
        let cancel_soon = async {
            tokio::task::yield_now().await;
            sender.send(true).unwrap();
        };
        let (allowed, ()) = tokio::time::timeout(Duration::from_millis(500), async {
            tokio::join!(waiting, cancel_soon)
        })
        .await
        .unwrap();
        assert!(!allowed);
        let (sender, cancel) = watch::channel(false);
        drop(sender);
        assert!(!wait_for_switch(Duration::from_secs(30), cancel).await);
        let (_sender, cancel) = watch::channel(false);
        assert!(wait_for_switch(Duration::ZERO, cancel).await);
    }

    #[test]
    fn insertion_outcomes_do_not_claim_confirmation_for_a_sent_or_skipped_paste() {
        assert_eq!(
            insertion_message(InsertResult::ClipboardPaste),
            "Insertion test completed."
        );
        assert!(insertion_message(InsertResult::ClipboardOnly).contains("skipped"));
        assert!(insertion_message(InsertResult::PasteUnverified).contains("Check the target app"));
    }

    #[test]
    fn setup_failure_copy_is_registered_and_does_not_echo_native_errors() {
        for error in [
            "Choose another app for the insertion test.",
            "private native path sentinel",
        ] {
            let message = insertion_failure_message(error);
            assert!(crate::messages::code(message).is_some());
            assert!(!message.contains("sentinel"));
        }
    }
}

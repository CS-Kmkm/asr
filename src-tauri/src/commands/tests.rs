#![cfg(test)]

use super::*;

#[test]
fn debug_builds_never_change_the_release_autostart_registration() {
    for desired in [false, true] {
        assert_eq!(
            autostart_update(desired, desired, false),
            AutostartUpdate::Unchanged
        );
        assert_eq!(
            autostart_update(desired, desired, true),
            AutostartUpdate::Unchanged
        );
        assert_eq!(
            autostart_update(!desired, desired, false),
            AutostartUpdate::Apply(desired)
        );
        assert_eq!(
            autostart_update(!desired, desired, true),
            AutostartUpdate::SkippedInDebugBuild
        );
    }
}

#[test]
fn blank_ask_input_is_rejected_before_planning() {
    for spoken in ["", "  ", "\n\t"] {
        assert_eq!(
            require_ask_spoken_input(spoken),
            Err("No Ask instruction was captured.".to_string())
        );
    }
    assert!(require_ask_spoken_input("search Google for Rust").is_ok());
}
use crate::audio::{AudioArtifact, AudioError, AudioFuture, CaptureState, LevelMeter};
use std::sync::atomic::AtomicUsize;

#[test]
fn retry_dictate_label_tracks_correction_even_when_text_is_unchanged() {
    assert_eq!(
        retry_dictate_history_mode(Some("local"), false),
        "ai_corrected"
    );
    assert_eq!(retry_dictate_history_mode(None, false), "faithful");
    assert_eq!(retry_dictate_history_mode(None, true), "faithful_fallback");
}

#[test]
fn retry_announces_the_voice_mode_of_its_history_row() {
    let row = |mode: &str, target_language: Option<&str>| HistoryItem {
        id: 1,
        transcript_text: "text".into(),
        processed_text: None,
        source_text: None,
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: mode.into(),
        asr_provider: "mock".into(),
        llm_provider: None,
        target_language: target_language.map(str::to_owned),
        app_category: None,
        duration_ms: None,
        latency_ms: None,
        created_at: String::new(),
        has_audio: true,
        retry_of_id: None,
        insertion_result: None,
        insertion_detail: None,
        expires_at: None,
    };
    for (mode, target_language, event_mode) in [
        ("faithful", None, "dictate"),
        ("ai_corrected", None, "dictate"),
        ("faithful_fallback", None, "dictate"),
        ("translate", Some("ja"), "translate"),
        ("edit", None, "edit"),
        ("ask", None, "ask"),
    ] {
        let pipeline_mode = history_pipeline_mode(mode).unwrap();
        assert_eq!(
            retry_voice_mode(pipeline_mode, &row(mode, target_language)),
            serde_json::json!({"mode": event_mode, "targetLanguage": target_language}),
            "{mode}"
        );
    }
}

#[test]
fn edit_failures_name_their_cause() {
    use correction::CorrectionError;
    use reqwest::StatusCode;
    let api = |status| CorrectionError::Api {
        status,
        message: "details".into(),
    };
    let messages = [
        edit_failure_message(&CorrectionError::MissingApiKey("OPENAI_API_KEY".into())),
        edit_failure_message(&api(StatusCode::UNAUTHORIZED)),
        edit_failure_message(&api(StatusCode::TOO_MANY_REQUESTS)),
        edit_failure_message(&api(StatusCode::INTERNAL_SERVER_ERROR)),
        edit_failure_message(&CorrectionError::InvalidEndpoint("ftp://".into())),
        edit_failure_message(&CorrectionError::OutputLimit),
        edit_failure_message(&CorrectionError::ProviderOutputLimit),
        edit_failure_message(&CorrectionError::InvalidResponse("empty".into())),
        edit_failure_message(&CorrectionError::UnsupportedProvider("other".into())),
        edit_failure_message(&CorrectionError::EmptyEditInstruction),
    ];
    let distinct = messages.iter().collect::<std::collections::HashSet<_>>();
    assert_eq!(distinct.len(), messages.len());
    assert_eq!(
        edit_failure_message(&api(StatusCode::FORBIDDEN)),
        edit_failure_message(&api(StatusCode::UNAUTHORIZED))
    );
    // Provider details may echo request content, so they are never shown.
    assert!(messages.iter().all(|message| !message.contains("details")));
}

#[test]
fn retry_keeps_the_transcript_when_correction_fails_or_is_rejected() {
    assert!(matches!(
        classify_ai_stage(Ok("corrected".into())),
        AiStageResult::Corrected(text) if text == "corrected"
    ));
    assert!(matches!(
        classify_ai_stage(Err(correction::CorrectionError::Cancelled)),
        AiStageResult::Cancelled
    ));
    for error in [
        correction::CorrectionError::ProtectedContentChanged(
            correction::ProtectedContentKind::Name,
        ),
        correction::CorrectionError::OutputLimit,
        correction::CorrectionError::MissingApiKey("OPENAI_API_KEY".into()),
        correction::CorrectionError::InvalidResponse("empty".into()),
    ] {
        assert!(matches!(
            classify_ai_stage(Err(error)),
            AiStageResult::Fallback(_)
        ));
    }
}

#[test]
fn retry_routes_personalization_by_the_saved_category_only() {
    let item = |app_category: Option<&str>| HistoryItem {
        id: 1,
        transcript_text: "text".into(),
        processed_text: None,
        source_text: None,
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: "faithful".into(),
        asr_provider: "mock".into(),
        llm_provider: None,
        target_language: None,
        app_category: app_category.map(str::to_owned),
        duration_ms: None,
        latency_ms: None,
        created_at: String::new(),
        has_audio: true,
        retry_of_id: None,
        insertion_result: None,
        insertion_detail: None,
        expires_at: None,
    };
    let profile = |formality: &str| types::StyleProfile {
        formality: formality.into(),
        detail: "concise".into(),
        guidance: None,
    };
    let settings = Settings {
        personalization_enabled: true,
        global_style_profile: Some(profile("formal")),
        scoped_style_profiles: vec![
            types::ScopedStyleProfile {
                scope: "category:messaging".into(),
                profile: profile("casual"),
            },
            types::ScopedStyleProfile {
                scope: "app:slack".into(),
                profile: profile("formal"),
            },
        ],
        ..Settings::default()
    };

    let messaging = retry_app_context(&item(Some("messaging")));
    assert_eq!(
        messaging,
        Some(types::AppContext {
            app_key: None,
            category: "messaging".into(),
        })
    );
    assert_eq!(
        personalization::resolve_profile(&settings, messaging.as_ref())
            .unwrap()
            .formality,
        "casual"
    );
    let unknown = retry_app_context(&item(None));
    assert_eq!(unknown, None);
    assert_eq!(
        personalization::resolve_profile(&settings, unknown.as_ref())
            .unwrap()
            .formality,
        "formal"
    );
}

#[test]
fn history_database_failure_is_reported_without_failing_completed_output() {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("history-save-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let database = dir.join("test.db");
    let storage = Storage::open(&database).unwrap();
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("DROP TABLE dictation_history", [])
        .unwrap();
    let status = save_history(
        &storage,
        &NewHistoryItem {
            transcript_text: "completed output",
            processed_text: None,
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: "faithful",
            asr_provider: "test",
            llm_provider: None,
            target_language: None,
            app_category: None,
            duration_ms: None,
            latency_ms: None,
            retry_of_id: None,
            insertion_result: None,
            insertion_detail: None,
        },
        None,
    );
    assert_eq!(status, HistorySaveStatus::Failed);
    drop(storage);
    let _ = std::fs::remove_dir_all(dir);
}

fn settings_history_item() -> NewHistoryItem<'static> {
    NewHistoryItem {
        transcript_text: "private transcript",
        processed_text: None,
        source_text: None,
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: "faithful",
        asr_provider: "test",
        llm_provider: None,
        target_language: None,
        app_category: None,
        duration_ms: None,
        latency_ms: None,
        retry_of_id: None,
        insertion_result: None,
        insertion_detail: None,
    }
}

fn stored_history_rows(database: &std::path::Path) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM dictation_history", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn update_settings_persistence_purges_history_in_the_settings_write() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("settings.sqlite");
    let storage = Storage::open(&database).unwrap();
    assert!(
        storage
            .add_history_with_audio_report(&settings_history_item(), None)
            .unwrap()
            .0
    );
    let settings = Settings {
        history_retention: types::HistoryRetention::Never,
        ..Settings::default()
    };
    persist_settings(&storage, &settings).unwrap();

    assert_eq!(
        storage.get_settings().unwrap().history_retention,
        types::HistoryRetention::Never
    );
    assert_eq!(stored_history_rows(&database), 0);
}

#[test]
fn selected_text_and_voice_shortcuts_swap_in_one_persisted_update() {
    let storage = Storage::in_memory().unwrap();
    let previous = storage.get_settings().unwrap();
    let old_routes = shortcuts::Routes::parse(&previous).unwrap();
    let mut settings = previous.clone();
    settings.translation_hotkey = previous.shortcuts.translate[0].clone();
    assert!(shortcuts::desired_routes_for_update(&previous, &settings, &old_routes).is_err());
    settings.shortcuts.translate[0] = previous.translation_hotkey.clone();
    let desired = shortcuts::desired_routes_for_update(&previous, &settings, &old_routes).unwrap();
    shortcuts::update_registrations(
        &old_routes,
        &desired,
        |_| -> Result<(), &str> { panic!("swapping existing chords needs no registration") },
        |_| -> Result<(), &str> { panic!("swapping existing chords needs no removal") },
        || persist_settings(&storage, &settings).map(drop),
    )
    .unwrap();
    let saved = storage.get_settings().unwrap();
    assert_eq!(saved.translation_hotkey, previous.shortcuts.translate[0]);
    assert_eq!(saved.shortcuts.translate[0], previous.translation_hotkey);
    let routes = shortcuts::Routes::parse(&saved).unwrap();
    assert_eq!(
        routes
            .find(crate::parse_shortcut(&saved.translation_hotkey).unwrap())
            .unwrap()
            .action,
        shortcuts::Action::SelectedText
    );
    assert_eq!(
        routes
            .find(crate::parse_shortcut(&saved.shortcuts.translate[0]).unwrap())
            .unwrap()
            .action,
        shortcuts::Action::Translate
    );
}

#[test]
fn failed_update_settings_write_keeps_history_and_settings() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("settings.sqlite");
    let storage = Storage::open(&database).unwrap();
    assert!(
        storage
            .add_history_with_audio_report(&settings_history_item(), None)
            .unwrap()
            .0
    );
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_settings_insert BEFORE INSERT ON settings
                 BEGIN SELECT RAISE(ABORT, 'settings write rejected'); END;
                 CREATE TRIGGER reject_settings_update BEFORE UPDATE ON settings
                 BEGIN SELECT RAISE(ABORT, 'settings write rejected'); END;",
        )
        .unwrap();
    let settings = Settings {
        history_retention: types::HistoryRetention::Never,
        ..Settings::default()
    };
    assert!(persist_settings(&storage, &settings).is_err());

    // The purge shares the failed settings transaction, so History-off
    // cannot erase rows while the stored settings stay unchanged. The
    // shortcut rollback on this error is covered by
    // `shortcuts::tests::persistence_failure_rolls_back_registrations`.
    assert_eq!(stored_history_rows(&database), 1);
    assert_eq!(
        storage.get_settings().unwrap().history_retention,
        Settings::default().history_retention
    );
}

#[test]
fn edit_persistence_failure_does_not_skip_metrics_or_propagate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edit.sqlite");
    let storage = Storage::open(&path).unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DROP TABLE dictation_history", [])
        .unwrap();
    let item = NewHistoryItem {
        transcript_text: "shorten this",
        processed_text: Some("short"),
        source_text: Some("long selection"),
        instruction_text: Some("shorten this"),
        action_kind: None,
        search_site: None,
        mode: "edit",
        asr_provider: "test",
        llm_provider: Some("local"),
        target_language: None,
        app_category: None,
        duration_ms: Some(100),
        latency_ms: Some(200),
        retry_of_id: None,
        insertion_result: None,
        insertion_detail: None,
    };
    assert_eq!(
        persist_edit_completion(&storage, &item, None, "local", 200),
        (HistorySaveStatus::Failed, false)
    );
}

#[test]
fn edit_replacement_failure_falls_back_to_clipboard_or_reports_no_outcome() {
    for result in [
        InsertResult::ClipboardPaste,
        InsertResult::ClipboardOnly,
        InsertResult::PasteUnverified,
    ] {
        let outcome = edit_insertion_outcome(Ok::<_, ()>(result), || -> Result<(), ()> {
            panic!("a completed replacement must not copy again")
        });
        assert_eq!(outcome, Some(result));
    }
    assert_eq!(
        edit_insertion_outcome(Err::<InsertResult, _>("replace"), || Ok::<_, ()>(())),
        Some(InsertResult::ClipboardOnly)
    );
    assert_eq!(
        edit_insertion_outcome(Err::<InsertResult, _>("replace"), || Err("clipboard")),
        None
    );
}

/// Speak to edit and Ask deliver their output after `stop_recording_for`
/// claimed Processing.
fn processing_after_recording(lifecycle: &PipelineLifecycle, mode: PipelineMode) -> u64 {
    let id = lifecycle.begin_start(mode).unwrap();
    lifecycle.mark_recording(id).unwrap();
    lifecycle.begin_processing().unwrap();
    id
}

#[test]
fn only_an_output_step_that_did_not_run_can_end_cancelled() {
    assert_eq!(
        committed_output(Ok(Ok("selection")), || -> bool {
            panic!("a step that ran is committed")
        }),
        CommittedOutput::Ran("selection")
    );
    assert_eq!(
        committed_output::<&str>(Ok(Err(OutputCancelled)), || false),
        CommittedOutput::Cancelled
    );
    assert_eq!(
        committed_output::<&str>(Err("pipeline operation was cancelled"), || true),
        CommittedOutput::Cancelled
    );
    assert_eq!(
        committed_output::<&str>(Err("pipeline operation was cancelled"), || false),
        CommittedOutput::Refused("pipeline operation was cancelled")
    );
}

#[test]
fn an_edit_cancelled_before_its_replacement_neither_pastes_nor_copies() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Edit);
    assert_eq!(
        lifecycle.cancel().unwrap(),
        Some((id, PipelinePhase::Processing))
    );
    assert_eq!(
        commit_edit_replacement(
            &lifecycle,
            id,
            || panic!("a cancelled edit must not paste"),
            || panic!("a cancelled edit must not copy"),
        ),
        CommittedOutput::Cancelled
    );
}

#[test]
fn a_committed_edit_keeps_its_outcome_after_a_later_cancel() {
    use injection::InjectionError;
    // (replacement, fallback copy, committed outcome)
    let cases = [
        (
            Ok(InsertResult::ClipboardPaste),
            Ok(()),
            (
                Some(InsertResult::ClipboardPaste),
                InsertionDetail::ClipboardUnavailable,
            ),
        ),
        (
            Ok(InsertResult::PasteUnverified),
            Ok(()),
            (
                Some(InsertResult::PasteUnverified),
                InsertionDetail::ClipboardUnavailable,
            ),
        ),
        (
            Err(InjectionError::ClipboardUnavailable),
            Ok(()),
            (
                Some(InsertResult::ClipboardOnly),
                InsertionDetail::ClipboardUnavailable,
            ),
        ),
        (
            Err(InjectionError::ClipboardProtected),
            Err(InjectionError::ClipboardProtected),
            (None, InsertionDetail::ClipboardProtected),
        ),
    ];
    for (replaced, copied, outcome) in cases {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Edit);
        let mut copy_attempted = false;
        let committed = commit_edit_replacement(
            &lifecycle,
            id,
            || replaced.clone(),
            || {
                copy_attempted = true;
                copied
            },
        );
        assert_eq!(committed, CommittedOutput::Ran(outcome));
        // Only a failure before any paste falls back to the clipboard.
        assert_eq!(copy_attempted, replaced.is_err());
        // The edit completes: a later cancel changes nothing.
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert!(!lifecycle.is_cancelled(id));
    }
}

#[test]
fn a_dictation_cancelled_before_its_output_neither_pastes_nor_copies() {
    // Dictate's final paste; voice translation's paste or fallback copy.
    for mode in [PipelineMode::Dictate, PipelineMode::Translate] {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, mode);
        assert_eq!(
            lifecycle.cancel().unwrap(),
            Some((id, PipelinePhase::Processing))
        );
        assert_eq!(
            commit_dictation_output(&lifecycle, id, || {
                panic!("a cancelled dictation must neither paste nor copy")
            }),
            CommittedOutput::Cancelled
        );
        assert!(lifecycle.is_cancelled(id));
    }
}

#[test]
fn a_dictation_output_that_saw_a_cancel_stays_cancellable() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Dictate);
    assert_eq!(
        commit_dictation_output(&lifecycle, id, || Err(injection::InjectionError::Cancelled)),
        CommittedOutput::Cancelled
    );
    assert_eq!(
        lifecycle.cancel().unwrap(),
        Some((id, PipelinePhase::Processing))
    );
}

#[test]
fn a_committed_dictation_output_completes_after_a_later_cancel() {
    use injection::test_backend::{self, MockBackend};
    use std::sync::{Arc, Mutex};
    // An insertion error is committed too: it is saved and reported as before.
    for output in [
        Ok(InsertResult::ClipboardPaste),
        Ok(InsertResult::PasteUnverified),
        Ok(InsertResult::ClipboardOnly),
        Err(injection::InjectionError::ClipboardUnavailable),
    ] {
        let lifecycle = PipelineLifecycle::default();
        let id = processing_after_recording(&lifecycle, PipelineMode::Translate);
        assert_eq!(
            commit_dictation_output(&lifecycle, id, || output.clone()),
            CommittedOutput::Ran(output)
        );
        assert_eq!(lifecycle.cancel().unwrap(), None);
        assert!(!lifecycle.is_cancelled(id));
    }
    // The final paste observes the same cancel the lifecycle sends.
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Dictate);
    let cancel = lifecycle.cancellation(id).unwrap();
    let mock = Arc::new(Mutex::new(MockBackend::new()));
    let injector = SystemTextInjector::with_mock(Arc::clone(&mock), InjectionOptions::default());
    let monitor = InputMonitor::default();
    monitor.test_set_available(true);
    monitor.observe_cancellation(Some(cancel.clone()));
    let checkpoint = monitor.checkpoint().unwrap();
    assert_eq!(
        commit_dictation_output(&lifecycle, id, || {
            injector.insert_final(
                "final",
                &test_backend::target(),
                &monitor,
                checkpoint,
                &cancel,
                false,
            )
        }),
        CommittedOutput::Ran(Ok(InsertResult::ClipboardPaste))
    );
    assert_eq!(lifecycle.cancel().unwrap(), None);
    // History and the insertion diagnosis never see a cancelled monitor.
    assert!(!*cancel.borrow());
    assert!(monitor.unchanged_since(checkpoint));
    assert_eq!(mock.lock().unwrap().content(), "prefix final suffix");
}

#[test]
fn an_ask_insertion_copies_only_after_a_failure_before_any_paste() {
    use injection::InjectionError;
    let no_copy =
        || -> Result<(), InjectionError> { panic!("a paste result or a cancel must not copy") };
    for pasted in ["selection", "caret"] {
        assert_eq!(
            ask_insertion(Some(Ok(InsertResult::ClipboardPaste)), pasted, no_copy),
            Ok(pasted)
        );
    }
    assert_eq!(
        ask_insertion(Some(Ok(InsertResult::ClipboardOnly)), "caret", no_copy),
        Ok("clipboard_only")
    );
    assert_eq!(
        ask_insertion(
            Some(Ok(InsertResult::PasteUnverified)),
            "selection",
            no_copy
        ),
        Ok("paste_unverified")
    );
    assert_eq!(
        ask_insertion(Some(Err(InjectionError::Cancelled)), "selection", no_copy),
        Err(OutputCancelled)
    );
    for inserted in [None, Some(Err(InjectionError::TargetChanged))] {
        assert_eq!(
            ask_insertion(inserted.clone(), "selection", || Ok(())),
            Ok("clipboard_only")
        );
        assert_eq!(
            ask_insertion(inserted.clone(), "caret", || {
                Err(InjectionError::ClipboardProtected)
            }),
            Ok("clipboard_protected")
        );
        assert_eq!(
            ask_insertion(inserted, "caret", || Err(
                InjectionError::ClipboardUnavailable
            )),
            Ok("insertion_failed")
        );
    }
}

#[test]
fn an_ask_cancelled_before_its_output_neither_pastes_nor_copies() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    lifecycle.cancel().unwrap();
    let mut delivered = false;
    assert_eq!(
        commit_ask_output(&lifecycle, id, || {
            delivered = true;
            Ok("clipboard_only")
        }),
        Err("ask was cancelled".to_string())
    );
    assert!(!delivered);
    assert!(lifecycle.is_cancelled(id));
}

#[test]
fn an_ask_output_that_saw_a_cancel_stays_cancellable() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    assert_eq!(
        commit_ask_output(&lifecycle, id, || Err(OutputCancelled)),
        Err("ask was cancelled".to_string())
    );
    assert_eq!(
        lifecycle.cancel().unwrap(),
        Some((id, PipelinePhase::Processing))
    );
}

#[test]
fn a_refused_ask_output_that_was_not_cancelled_reports_an_incomplete_operation() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    assert_eq!(
        commit_ask_output(&lifecycle, id, || Ok("selection")),
        Ok("selection")
    );
    // The operation is already committed, so a second output is refused.
    assert_eq!(
        commit_ask_output(
            &lifecycle,
            id,
            || -> Result<&'static str, OutputCancelled> { panic!("a refused output must not run") }
        ),
        Err(crate::state::OPERATION_INCOMPLETE.to_string())
    );
}

#[test]
fn a_cancel_during_an_ask_paste_waits_and_the_ask_completes() {
    use std::sync::{mpsc, Arc};
    let lifecycle = Arc::new(PipelineLifecycle::default());
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    let cancel = lifecycle.cancellation(id).unwrap();
    let (pasting, wait_pasting) = mpsc::channel();
    let (release, wait_release) = mpsc::channel::<()>();
    let inserter = {
        let lifecycle = lifecycle.clone();
        std::thread::spawn(move || {
            commit_ask_output(&lifecycle, id, || {
                // The paste is queued and still being verified.
                pasting.send(()).unwrap();
                wait_release.recv().unwrap();
                ask_insertion(
                    Some(Ok(InsertResult::PasteUnverified)),
                    "selection",
                    || -> Result<(), injection::InjectionError> {
                        panic!("a queued paste is never followed by a copy")
                    },
                )
            })
        })
    };
    wait_pasting.recv().unwrap();
    let canceller = {
        let lifecycle = lifecycle.clone();
        std::thread::spawn(move || lifecycle.cancel().unwrap())
    };
    std::thread::sleep(Duration::from_millis(20));
    assert!(!canceller.is_finished(), "a cancel must wait for the paste");
    release.send(()).unwrap();
    assert_eq!(inserter.join().unwrap(), Ok("paste_unverified"));
    assert_eq!(canceller.join().unwrap(), None);
    // `finish_ask` therefore saves History and publishes completion.
    assert!(!*cancel.borrow());
    assert!(!lifecycle.is_cancelled(id));
}

#[test]
fn an_ask_clarification_cancelled_before_its_copy_does_not_copy() {
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    lifecycle.cancel().unwrap();
    assert_eq!(
        commit_ask_copy(&lifecycle, id, || panic!("a cancelled Ask must not copy")),
        Err("ask was cancelled".to_string())
    );
}

#[test]
fn an_ask_clarification_copy_commits_and_a_failed_copy_stays_cancellable() {
    use injection::InjectionError;
    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    assert_eq!(commit_ask_copy(&lifecycle, id, || Ok(())), Ok(()));
    assert_eq!(lifecycle.cancel().unwrap(), None);
    assert!(!lifecycle.is_cancelled(id));

    let lifecycle = PipelineLifecycle::default();
    let id = processing_after_recording(&lifecycle, PipelineMode::Ask);
    assert_eq!(
        commit_ask_copy(&lifecycle, id, || Err(InjectionError::ClipboardUnavailable)),
        Err(command_error(InjectionError::ClipboardUnavailable))
    );
    assert_eq!(
        lifecycle.cancel().unwrap(),
        Some((id, PipelinePhase::Processing))
    );
}

#[test]
fn edit_insertion_failure_retains_generated_text_and_failure_metric() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edit-insertion-failure.sqlite");
    let storage = Storage::open(&path).unwrap();
    let item = NewHistoryItem {
        transcript_text: "shorten this",
        processed_text: Some("short"),
        source_text: Some("long selection"),
        instruction_text: Some("shorten this"),
        action_kind: None,
        search_site: None,
        mode: "edit",
        asr_provider: "test",
        llm_provider: Some("local"),
        target_language: None,
        app_category: None,
        duration_ms: Some(100),
        latency_ms: Some(200),
        retry_of_id: None,
        insertion_result: Some("insertion_failed"),
        insertion_detail: Some("clipboard_unavailable"),
    };
    assert_eq!(
        persist_edit_completion(&storage, &item, None, "local", 200),
        (HistorySaveStatus::Complete, false)
    );
    let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].processed_text.as_deref(), Some("short"));
    assert_eq!(rows[0].source_text.as_deref(), Some("long selection"));
    assert_eq!(
        rows[0].insertion_result.as_deref(),
        Some("insertion_failed")
    );
    assert_eq!(
        rows[0].insertion_detail.as_deref(),
        Some("clipboard_unavailable")
    );
    let metric: (bool, String) = rusqlite::Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT success, error_code FROM metrics WHERE event_type = 'speak_to_edit'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metric, (false, "insertion_failed".into()));
}

#[test]
fn edit_persistence_warning_claims_completion_only_after_a_confirmed_paste() {
    for insertion in [
        InsertResult::ClipboardPaste,
        InsertResult::ClipboardOnly,
        InsertResult::PasteUnverified,
    ] {
        assert_eq!(edit_persistence_warning(insertion, false, false), None);
        for (history_failed, metric_failed, kind) in [
            (true, true, "history_metric_save_failed"),
            (true, false, "history_save_failed"),
            (false, true, "metric_save_failed"),
        ] {
            let (actual_kind, message) =
                edit_persistence_warning(insertion, history_failed, metric_failed).unwrap();
            assert_eq!(actual_kind, kind);
            if insertion == InsertResult::ClipboardPaste {
                assert!(message.starts_with("The edit was completed"), "{message}");
            } else {
                assert!(!message.contains("completed"), "{message}");
                assert!(message.contains("remains on the clipboard"), "{message}");
            }
            assert_eq!(message.contains("history"), history_failed, "{message}");
            assert_eq!(
                message.contains("usage metrics"),
                metric_failed,
                "{message}"
            );
        }
    }
}

#[test]
fn persistence_failures_choose_one_complete_warning() {
    assert_eq!(persistence_warning(false, false), None);
    assert_eq!(
        persistence_warning(true, false).unwrap().0,
        "history_save_failed"
    );
    assert_eq!(
        persistence_warning(false, true).unwrap().0,
        "metric_save_failed"
    );
    assert_eq!(
        persistence_warning(true, true).unwrap().0,
        "history_and_metric_save_failed"
    );
    assert_eq!(
        completion_persistence_warning(HistorySaveStatus::AudioUnavailable, true)
            .unwrap()
            .0,
        "history_audio_and_metric_save_failed"
    );
    assert_eq!(
        completion_persistence_warning(HistorySaveStatus::AudioUnavailable, false)
            .unwrap()
            .0,
        "history_audio_unavailable"
    );
}

#[test]
fn translation_targets_require_supported_unique_current_language() {
    let mut settings = Settings::default();
    assert!(validate_translation_targets(&settings).is_ok());

    settings.translation_target_languages = vec!["en".into(), "en".into()];
    assert!(validate_translation_targets(&settings).is_err());

    settings.translation_target_languages = vec!["xx".into()];
    settings.translation_target_language = "xx".into();
    assert!(validate_translation_targets(&settings).is_err());

    settings.translation_target_languages = vec!["ja".into()];
    settings.translation_target_language = "en".into();
    assert!(validate_translation_targets(&settings).is_err());
}

#[test]
fn next_translation_target_wraps_and_falls_back_to_the_first_entry() {
    let languages = vec!["en".to_string(), "ja".to_string(), "de".to_string()];
    assert_eq!(
        next_translation_target(&languages, "ja").as_deref(),
        Some("de")
    );
    assert_eq!(
        next_translation_target(&languages, "de").as_deref(),
        Some("en")
    );
    assert_eq!(
        next_translation_target(&languages, "xx").as_deref(),
        Some("ja")
    );
    assert_eq!(next_translation_target(&[], "en"), None);
}

#[test]
fn target_cycling_waits_for_an_in_progress_settings_save() {
    let storage = Arc::new(Storage::in_memory().unwrap());
    let settings_save = storage.lock_settings_writes().unwrap();
    let cycling = {
        let storage = Arc::clone(&storage);
        std::thread::spawn(move || store_next_translation_target(&storage, Some("en")))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!cycling.is_finished());

    // The save was based on settings read before cycling started.
    let mut saved = storage.get_settings().unwrap();
    saved.history_retention = types::HistoryRetention::Never;
    storage.update_settings(&saved).unwrap();
    drop(settings_save);

    let cycled = cycling.join().unwrap().unwrap();
    assert_eq!(cycled.settings.translation_target_language, "ja");
    let stored = storage.get_settings_snapshot().unwrap();
    assert_eq!(stored, cycled);
    assert_eq!(
        stored.settings.history_retention,
        types::HistoryRetention::Never
    );
}

#[test]
fn cycling_the_target_advances_the_settings_revision() {
    let storage = Storage::in_memory().unwrap();
    let before = storage.get_settings_snapshot().unwrap();
    let cycled = store_next_translation_target(&storage, None).unwrap();
    assert_eq!(cycled.revision, before.revision + 1);
    assert_eq!(cycled.settings.translation_target_language, "ja");
    assert_eq!(storage.get_settings_snapshot().unwrap(), cycled);
}

/// The storage steps of `update_settings`: under the settings-write lock,
/// apply the patch to the stored settings, validate, and persist.
fn save_settings_patch(
    storage: &Storage,
    patch: serde_json::Value,
) -> Result<types::SettingsSnapshot, String> {
    let _settings_writes = storage.lock_settings_writes().unwrap();
    let previous = storage.get_settings().unwrap();
    let serde_json::Value::Object(patch) = patch else {
        panic!("a patch is a JSON object");
    };
    let settings = apply_settings_patch(&previous, patch)?;
    settings.validate()?;
    let revision = persist_settings(storage, &settings)?;
    Ok(types::SettingsSnapshot { settings, revision })
}

#[test]
fn a_settings_patch_advances_the_revision_and_returns_the_stored_settings() {
    let storage = Storage::in_memory().unwrap();
    let before = storage.get_settings_snapshot().unwrap();
    let saved =
        save_settings_patch(&storage, serde_json::json!({ "interactionSounds": true })).unwrap();
    assert_eq!(saved.revision, before.revision + 1);
    assert_eq!(
        saved.settings,
        Settings {
            interaction_sounds: true,
            ..before.settings
        }
    );
    assert_eq!(storage.get_settings_snapshot().unwrap(), saved);
}

#[test]
fn changing_speech_endpoints_revokes_old_speech_consents() {
    let mut previous = Settings {
        asr_backend: "openai-compatible".into(),
        ..Settings::default()
    };
    previous.cloud_consents = vec!["openai".into(), crate::privacy::asr_consent_key(&previous)];
    let patch = serde_json::json!({"apiBaseUrl": "https://another.example/v1"});
    let next = apply_settings_patch(&previous, patch.as_object().unwrap().clone()).unwrap();
    assert_eq!(next.cloud_consents, vec!["openai"]);
    let back = serde_json::json!({"apiBaseUrl": previous.api_base_url});
    let restored = apply_settings_patch(&next, back.as_object().unwrap().clone()).unwrap();
    assert_eq!(restored.cloud_consents, vec!["openai"]);
    let local = serde_json::json!({"asrBackend": "faster-whisper"});
    assert_eq!(
        apply_settings_patch(&previous, local.as_object().unwrap().clone())
            .unwrap()
            .cloud_consents,
        vec!["openai"]
    );
}

#[test]
fn a_settings_patch_keeps_a_target_cycled_after_the_window_read_its_settings() {
    let storage = Storage::in_memory().unwrap();
    // The window shows these settings, then a voice translation cycles
    // the target before the window's queued save is sent.
    let shown = storage.get_settings_snapshot().unwrap();
    let cycled = store_next_translation_target(&storage, None).unwrap();
    assert_ne!(
        cycled.settings.translation_target_language,
        shown.settings.translation_target_language
    );
    let saved = save_settings_patch(
        &storage,
        serde_json::json!({ "historyRetention": "never", "theme": "dark" }),
    )
    .unwrap();
    assert_eq!(saved.revision, cycled.revision + 1);
    assert_eq!(
        saved.settings.translation_target_language,
        cycled.settings.translation_target_language
    );
    assert_eq!(
        saved.settings.history_retention,
        types::HistoryRetention::Never
    );
    assert_eq!(saved.settings.theme, "dark");
    assert_eq!(storage.get_settings_snapshot().unwrap(), saved);
}

#[test]
fn a_settings_patch_with_an_unknown_key_or_invalid_value_writes_nothing() {
    let storage = Storage::in_memory().unwrap();
    let before = storage.get_settings_snapshot().unwrap();
    for patch in [
        // A key that is not a settings field, even beside a valid one.
        serde_json::json!({ "theme": "dark", "futureSetting": true }),
        // Legacy scalar mirrors are not settings fields either.
        serde_json::json!({ "hotkey": "Ctrl+Alt+V" }),
        // Values that do not decode.
        serde_json::json!({ "interactionSounds": "yes" }),
        serde_json::json!({ "historyRetention": null }),
        serde_json::json!({ "shortcuts": { "dictate": ["Ctrl+Alt+V"] } }),
        // Values that decode but fail validation.
        serde_json::json!({ "theme": "sepia" }),
        serde_json::json!({ "inputGainPercent": 10 }),
        serde_json::json!({ "translationTargetLanguage": "xx" }),
    ] {
        assert!(
            save_settings_patch(&storage, patch.clone()).is_err(),
            "{patch} was accepted"
        );
        assert_eq!(storage.get_settings_snapshot().unwrap(), before);
    }
}

struct TestAudio {
    cancel_error: bool,
    disarm_error: bool,
    disarms: Arc<AtomicUsize>,
    stream_error: Option<String>,
}

impl AudioCapture for TestAudio {
    fn snapshot(&self, _: Duration) -> Result<crate::audio::AudioSnapshot, AudioError> {
        Err(AudioError::NotCapturing)
    }

    fn list_devices(&self) -> AudioFuture<'_, Vec<AudioDevice>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn arm(&mut self, _config: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn disarm(&mut self) -> AudioFuture<'_, ()> {
        self.disarms.fetch_add(1, Ordering::SeqCst);
        let error = self.disarm_error;
        Box::pin(async move {
            if error {
                Err(AudioError::StreamFailure("disarm failed".into()))
            } else {
                Ok(())
            }
        })
    }

    fn start(&mut self, _config: CaptureConfig) -> AudioFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&mut self) -> AudioFuture<'_, AudioArtifact> {
        Box::pin(async { Err(AudioError::NotCapturing) })
    }

    fn cancel(&mut self) -> AudioFuture<'_, ()> {
        let cancel_error = self.cancel_error;
        Box::pin(async move {
            if cancel_error {
                Err(AudioError::NotCapturing)
            } else {
                Ok(())
            }
        })
    }

    fn state(&self) -> CaptureState {
        CaptureState::Idle
    }

    fn level(&self) -> LevelMeter {
        LevelMeter::default()
    }

    fn stream_error(&self) -> Option<String> {
        self.stream_error.clone()
    }
}

fn test_audio(cancel_error: bool) -> tokio::sync::Mutex<Box<dyn AudioCapture>> {
    tokio::sync::Mutex::new(Box::new(TestAudio {
        cancel_error,
        disarm_error: false,
        disarms: Arc::new(AtomicUsize::new(0)),
        stream_error: None,
    }))
}

#[tokio::test]
async fn microphone_test_release_covers_stop_error_and_recording_start() {
    for from_error in [false, true] {
        let disarms = Arc::new(AtomicUsize::new(0));
        let mut audio = TestAudio {
            cancel_error: false,
            disarm_error: false,
            disarms: Arc::clone(&disarms),
            stream_error: from_error.then(|| "device disconnected".into()),
        };
        let mut test = MicrophoneTestState::default();
        let generation = test.start();
        if from_error {
            assert_eq!(audio.stream_error().as_deref(), Some("device disconnected"));
        }
        // stop_microphone_test, recording start, and stream-error cleanup
        // all use this release path before another capture can start.
        assert!(release_microphone_test(&mut test, &mut audio)
            .await
            .unwrap());
        assert!(!test.is_current(generation));
        assert!(!release_microphone_test(&mut test, &mut audio)
            .await
            .unwrap());
        assert_eq!(disarms.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn hiding_the_main_window_releases_a_running_microphone_test() {
    let disarms = Arc::new(AtomicUsize::new(0));
    let audio: tokio::sync::Mutex<Box<dyn AudioCapture>> =
        tokio::sync::Mutex::new(Box::new(TestAudio {
            cancel_error: false,
            disarm_error: false,
            disarms: Arc::clone(&disarms),
            stream_error: None,
        }));
    let test = tokio::sync::Mutex::new(MicrophoneTestState::default());
    let generation = test.lock().await.start();
    assert!(release_running_microphone_test(&test, &audio)
        .await
        .unwrap());
    // The meter task stops at its next tick once its generation is stale.
    assert!(!test.lock().await.is_current(generation));
    // Closing again with no test running leaves the device alone.
    assert!(!release_running_microphone_test(&test, &audio)
        .await
        .unwrap());
    assert_eq!(disarms.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_microphone_release_keeps_ownership_for_retry() {
    let disarms = Arc::new(AtomicUsize::new(0));
    let mut audio = TestAudio {
        cancel_error: false,
        disarm_error: true,
        disarms: Arc::clone(&disarms),
        stream_error: None,
    };
    let mut test = MicrophoneTestState::default();
    let generation = test.start();
    assert!(release_microphone_test(&mut test, &mut audio)
        .await
        .is_err());
    assert!(test.is_current(generation));
    audio.disarm_error = false;
    assert!(release_microphone_test(&mut test, &mut audio)
        .await
        .unwrap());
    assert!(!test.is_current(generation));
    assert_eq!(disarms.load(Ordering::SeqCst), 2);
}

#[test]
fn interaction_sound_gate_dispatches_only_enabled_cues() {
    let mut cues = 0;
    dispatch_interaction_sound(false, || cues += 1);
    dispatch_interaction_sound(true, || cues += 1);
    assert_eq!(cues, 1);
}

#[tokio::test]
async fn start_cue_finishes_before_capture_opens() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let cue_events = Arc::clone(&events);
    play_start_cue_before_capture(true, move || async move {
        cue_events.lock().unwrap().push("cue started");
        tokio::time::sleep(Duration::from_millis(30)).await;
        cue_events.lock().unwrap().push("cue finished");
    })
    .await;
    // start_recording_mode arms the microphone only after the helper returns.
    events.lock().unwrap().push("capture armed");
    assert_eq!(
        *events.lock().unwrap(),
        ["cue started", "cue finished", "capture armed"]
    );
}

#[tokio::test]
async fn disabled_start_cue_is_not_played_and_does_not_delay_capture() {
    let played = Arc::new(AtomicUsize::new(0));
    let cue_played = Arc::clone(&played);
    let started = Instant::now();
    play_start_cue_before_capture(false, move || async move {
        cue_played.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(5)).await;
    })
    .await;
    assert_eq!(played.load(Ordering::SeqCst), 0);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn startup_shortcut_warning_names_inactive_actions() {
    assert_eq!(startup_shortcut_warning(&[]), None);
    assert_eq!(
            startup_shortcut_warning(&[
                "Voice Translate (Ctrl+Shift+Y)".into(),
                "Ask Anything (Ctrl+Shift+A)".into(),
            ])
            .unwrap(),
            "Some saved hotkeys overlap or could not be registered. Change them in Settings. Inactive: Voice Translate (Ctrl+Shift+Y), Ask Anything (Ctrl+Shift+A)"
        );
}

#[tokio::test]
async fn immediate_stop_waits_for_session_publication() {
    let live = Arc::new(tokio::sync::Mutex::new(None));
    let edit = Arc::new(tokio::sync::Mutex::new(None));
    let ask = Arc::new(tokio::sync::Mutex::new(None));
    let translation_target = Arc::new(Mutex::new(None));
    let mut startup_publication = live.lock().await;
    let live_for_stop = Arc::clone(&live);
    let edit_for_stop = Arc::clone(&edit);
    let ask_for_stop = Arc::clone(&ask);
    let target_for_stop = Arc::clone(&translation_target);
    let stopping = tokio::spawn(async move {
        take_published_sessions(
            &live_for_stop,
            &edit_for_stop,
            &ask_for_stop,
            &target_for_stop,
        )
        .await
        .unwrap()
    });

    tokio::task::yield_now().await;
    *translation_target.lock().unwrap() = Some("ja".into());
    *startup_publication = Some(());
    drop(startup_publication);

    let (session, edit_session, ask_session, target) = stopping.await.unwrap();
    assert_eq!(session, Some(()));
    assert_eq!(edit_session, None::<()>);
    assert!(ask_session.is_none());
    assert_eq!(target.as_deref(), Some("ja"));
}

#[tokio::test(flavor = "current_thread")]
async fn aborting_stop_waiter_does_not_release_its_detached_blocking_finalizer() {
    let lifecycle = Arc::new(PipelineLifecycle::default());
    let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    lifecycle.mark_recording(id).unwrap();
    lifecycle.begin_processing().unwrap();
    let state = Arc::new(AppState::default());
    state.transition(AppPhase::Processing, None);
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (released, release) = std::sync::mpsc::channel();
    let (done, finished) = tokio::sync::oneshot::channel();
    let work_lifecycle = Arc::clone(&lifecycle);
    let work_state = Arc::clone(&state);
    let waiter = tokio::spawn(async move {
        spawn_owned_stop(async move {
            {
                let _owner = PipelineGuard {
                    lifecycle: &work_lifecycle,
                    id,
                };
                let _terminal = crate::state::TerminalPhaseGuard::new(
                    &work_state,
                    &work_lifecycle,
                    id,
                    "Dictation cancelled.",
                    |phase, message| {
                        work_state.transition(phase, Some(message.into()));
                    },
                );
                let worker_lifecycle = Arc::clone(&work_lifecycle);
                let result = crate::live_dictation::spawn_recording_owner(async move {
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(3)).unwrap();
                    worker_lifecycle.commit_side_effect(id, || -> Result<(), String> {
                        panic!("cancelled finalizer must not paste")
                    })
                })
                .await
                .unwrap();
                assert!(result.is_err());
            }
            done.send(()).unwrap();
        })
        .await
        .unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_eq!(lifecycle.phase(), PipelinePhase::Processing);
    assert_eq!(state.phase(), Some(AppPhase::Processing));
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
    lifecycle.cancel_for(Some(id)).unwrap();
    released.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), finished)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    assert_eq!(state.phase(), Some(AppPhase::Idle));
    let next = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    assert_ne!(id, next);
    assert!(!lifecycle.is_cancelled(next));
}

#[tokio::test]
async fn cancel_waits_for_audio_operation_then_allows_new_start() {
    let lifecycle = PipelineLifecycle::default();
    let operation_id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    lifecycle.mark_recording(operation_id).unwrap();
    let audio = test_audio(false);
    let target = Mutex::new(None);
    let audio_operation = audio.lock().await;
    let live = tokio::sync::Mutex::new(None);
    let edit = tokio::sync::Mutex::new(None);
    let ask = tokio::sync::Mutex::new(None);
    let language = Mutex::new(None);
    let cancellation = cancel_pipeline_operation(
        &lifecycle,
        &audio,
        &target,
        &language,
        &live,
        &edit,
        &ask,
        || (),
    );
    tokio::pin!(cancellation);

    assert!(
        tokio::time::timeout(Duration::from_millis(10), cancellation.as_mut())
            .await
            .is_err()
    );
    assert!(cancel_pipeline_operation(
        &lifecycle,
        &audio,
        &target,
        &language,
        &live,
        &edit,
        &ask,
        || ()
    )
    .await
    .unwrap()
    .is_none());
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
    drop(audio_operation);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), cancellation)
            .await
            .unwrap(),
        Ok(Some(PipelinePhase::Recording))
    );
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
}

#[tokio::test]
async fn failed_audio_cancel_still_allows_new_start() {
    let lifecycle = PipelineLifecycle::default();
    let operation_id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    lifecycle.mark_recording(operation_id).unwrap();
    let audio = test_audio(true);
    let target = Mutex::new(None);
    let published = std::cell::Cell::new(None);

    assert!(cancel_pipeline_operation(
        &lifecycle,
        &audio,
        &target,
        &Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        || published.set(Some(lifecycle.phase())),
    )
    .await
    .is_err());
    // The failed cancel still published its Idle phase, while it owned
    // the lifecycle, so no newer recording could be overwritten.
    assert_eq!(published.get(), Some(PipelinePhase::Recording));
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
}

#[tokio::test]
async fn processing_cancel_retains_ownership_until_processing_exits() {
    let lifecycle = PipelineLifecycle::default();
    let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    lifecycle.mark_recording(id).unwrap();
    let (_, _, cancel) = lifecycle.begin_processing().unwrap();
    let result = cancel_pipeline_operation(
        &lifecycle,
        &test_audio(false),
        &Mutex::new(None),
        &Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        || panic!("Processing keeps its owner, which publishes its phase"),
    )
    .await
    .unwrap();
    assert_eq!(result, Some(PipelinePhase::Processing));
    assert!(*cancel.borrow());
    assert!(lifecycle.begin_start(PipelineMode::Translate).is_err());
    lifecycle.finish(id);
    assert!(lifecycle.begin_start(PipelineMode::Translate).is_ok());
}

#[tokio::test]
async fn a_selected_text_translation_cancel_is_left_to_its_owner() {
    let lifecycle = PipelineLifecycle::default();
    let (id, cancel) = MODEL_CHANGE_GATE
        .begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::SelectedText))
        .unwrap();
    assert_eq!(lifecycle.mode(), Some(PipelineMode::SelectedText));
    let result = cancel_pipeline_operation(
        &lifecycle,
        &test_audio(false),
        &Mutex::new(None),
        &Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        &tokio::sync::Mutex::new(None),
        || panic!("the translation publishes its own Idle phase"),
    )
    .await
    .unwrap();
    assert_eq!(result, Some(PipelinePhase::Processing));
    // The request sees the cancel, the replacement is refused, and no
    // voice mode starts until the owner finishes.
    assert!(*cancel.borrow());
    assert!(lifecycle.is_cancelled(id));
    let mut replaced = false;
    assert!(lifecycle
        .commit_side_effect(id, || {
            replaced = true;
            Ok::<(), ()>(())
        })
        .is_err());
    assert!(!replaced);
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_err());
    lifecycle.finish(id);
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
}

#[test]
fn a_failed_translation_does_not_claim_the_clipboard() {
    assert!(!TRANSLATION_FAILED_SELECTION_UNCHANGED.contains("clipboard"));
    assert!(TRANSLATION_KEPT_IN_APP.contains("available in this app"));
}

#[test]
fn an_unverified_paste_is_not_reported_as_a_target_change() {
    let (kind, message) = selection_translation_status(InsertResult::PasteUnverified);
    assert_eq!(kind, "paste_unverified");
    assert!(!message.contains("target changed"), "{message}");
    assert!(message.contains("only if the text did not appear"));
    assert_eq!(
        selection_translation_status(InsertResult::ClipboardOnly).1,
        "The target changed; the translation remains on the clipboard."
    );
    assert_eq!(
        selection_translation_status(InsertResult::ClipboardPaste),
        ("success", "Translation inserted.")
    );
}

#[test]
fn selected_text_translation_ends_in_a_terminal_phase() {
    for result in [
        InsertResult::ClipboardPaste,
        InsertResult::PasteUnverified,
        InsertResult::ClipboardOnly,
    ] {
        assert_eq!(
            selected_text_terminal(SelectedTextEnd::Delivered(Some(result))),
            (AppPhase::Completed, selection_translation_status(result).1)
        );
    }
    assert_eq!(
        selected_text_terminal(SelectedTextEnd::Delivered(None)),
        (AppPhase::Error, TRANSLATION_KEPT_IN_APP)
    );
    // Every failure is a fixed sentence the overlay shows as a notice.
    for (failure, message) in [
        (
            SelectedTextFailure::SettingsUnavailable,
            "Translation settings are unavailable.",
        ),
        (
            SelectedTextFailure::NoSelection,
            "Select text in a supported foreground edit control.",
        ),
        (
            SelectedTextFailure::MonitorUnavailable,
            "Translation could not monitor the target safely.",
        ),
        (
            SelectedTextFailure::TranslationFailed,
            TRANSLATION_FAILED_SELECTION_UNCHANGED,
        ),
        (
            SelectedTextFailure::Incomplete,
            crate::state::OPERATION_INCOMPLETE,
        ),
    ] {
        let (phase, published) = selected_text_terminal(SelectedTextEnd::Failed(failure));
        assert_eq!((phase.clone(), published), (AppPhase::Error, message));
        assert_eq!(
            recording_overlay::phase_view(&phase),
            recording_overlay::PhaseView::Notice(recording_overlay::NoticeSeverity::Failure)
        );
    }
    assert_eq!(
        selected_text_terminal(SelectedTextEnd::Cancelled),
        (AppPhase::Idle, SELECTED_TEXT_TRANSLATION_CANCELLED)
    );
    assert_eq!(
        cancellation_copy(PipelineMode::SelectedText).0,
        SELECTED_TEXT_TRANSLATION_CANCELLED
    );
    // An early exit after a cancel publishes the same Idle message.
    assert_eq!(
        crate::state::unfinished_exit_phase(
            &AppPhase::Processing,
            true,
            cancellation_copy(PipelineMode::SelectedText).0
        ),
        Some((AppPhase::Idle, SELECTED_TEXT_TRANSLATION_CANCELLED))
    );
}

#[test]
fn selected_text_translation_records_one_metric_unless_cancelled() {
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Delivered(Some(
            InsertResult::ClipboardPaste
        ))),
        Some((true, None))
    );
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Delivered(Some(
            InsertResult::PasteUnverified
        ))),
        Some((false, Some("paste_unverified")))
    );
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Delivered(Some(
            InsertResult::ClipboardOnly
        ))),
        Some((false, Some("clipboard_only")))
    );
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Delivered(None)),
        Some((false, Some("insertion_failed")))
    );
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Failed(SelectedTextFailure::NoSelection)),
        Some((false, Some("no_selection")))
    );
    assert_eq!(
        selected_text_metric(SelectedTextEnd::Failed(
            SelectedTextFailure::TranslationFailed
        )),
        Some((false, Some("translation_failed")))
    );
    assert_eq!(selected_text_metric(SelectedTextEnd::Cancelled), None);
}

#[test]
fn selected_text_history_rows_hold_the_selection_and_its_translation() {
    let storage = Storage::in_memory().unwrap();
    let item = selected_text_history_item(
        "Guten Morgen",
        "Good morning",
        "openai",
        "en",
        Some("browser"),
        1234,
        selected_text_insertion_label(Some(InsertResult::ClipboardOnly)),
        Some("user_activity"),
    );
    assert_eq!(
        save_history(&storage, &item, None),
        HistorySaveStatus::Complete
    );
    let row = storage
        .list_history(types::HistoryFilter::Translate, 10)
        .unwrap()
        .remove(0);
    assert_eq!(row.mode, "selected_translate");
    assert_eq!(row.source_text.as_deref(), Some("Guten Morgen"));
    assert_eq!(row.processed_text.as_deref(), Some("Good morning"));
    assert_eq!(row.transcript_text, "");
    assert_eq!(row.instruction_text, None);
    assert_eq!(row.asr_provider, NO_ASR_PROVIDER);
    assert_eq!(row.llm_provider.as_deref(), Some("openai"));
    assert_eq!(row.target_language.as_deref(), Some("en"));
    assert_eq!(row.app_category.as_deref(), Some("browser"));
    assert_eq!(row.duration_ms, None);
    assert_eq!(row.latency_ms, Some(1234));
    assert_eq!(row.insertion_result.as_deref(), Some("clipboard_only"));
    assert_eq!(row.insertion_detail.as_deref(), Some("user_activity"));
    assert!(!row.has_audio);
    // It has no recording, so Retry refuses it.
    assert_eq!(
        history_pipeline_mode(&row.mode),
        Err("history mode is not retryable".to_string())
    );
    assert!(
        failed_take_history_item(PipelineMode::SelectedText, "mock", None, None, None, 1).is_none()
    );
    assert_eq!(
        selected_text_insertion_label(None),
        "insertion_failed",
        "the same code Speak to edit stores"
    );
}

#[test]
fn selected_text_history_honours_history_off() {
    let storage = Storage::in_memory().unwrap();
    persist_settings(
        &storage,
        &Settings {
            history_retention: types::HistoryRetention::Never,
            ..Settings::default()
        },
    )
    .unwrap();
    let item = selected_text_history_item(
        "source",
        "translation",
        "local",
        "ja",
        None,
        1,
        "clipboard_paste",
        None,
    );
    assert_eq!(
        save_history(&storage, &item, None),
        HistorySaveStatus::Complete
    );
    assert!(storage
        .list_history(types::HistoryFilter::All, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn only_recordings_fall_back_to_the_default_microphone() {
    let settings = Settings {
        microphone_id: Some("name:Headset".into()),
        ..Settings::default()
    };
    let recording = capture_config(&settings);
    assert_eq!(recording.device_id.as_deref(), Some("name:Headset"));
    assert!(recording.fallback_to_default);
    let test = microphone_test_config(Some("name:Headset".into()));
    assert_eq!(test.device_id.as_deref(), Some("name:Headset"));
    assert!(!test.fallback_to_default);
    assert_eq!(test.preroll, Duration::ZERO);
}

#[test]
fn capture_rate_matches_the_local_decoder() {
    for (backend, rate) in [
        ("faster-whisper", 16_000),
        ("vibevoice", 24_000),
        ("openai-compatible", 24_000),
    ] {
        let settings = Settings {
            asr_backend: backend.into(),
            ..Settings::default()
        };
        assert_eq!(capture_config(&settings).target_sample_rate, rate);
    }
}

#[test]
fn a_fallback_recording_names_the_default_microphone() {
    assert_eq!(
        recording_message(PipelineMode::Dictate, false),
        "Recording from the selected microphone."
    );
    assert_eq!(
        recording_message(PipelineMode::Dictate, true),
        "Recording from the default microphone."
    );
    for mode in [
        PipelineMode::Translate,
        PipelineMode::Edit,
        PipelineMode::Ask,
    ] {
        assert_eq!(
            recording_message(mode, true),
            recording_message(mode, false)
        );
    }
    assert!(!MICROPHONE_FALLBACK_HINT.is_empty());
    assert!(MICROPHONE_FALLBACK_STATUS.contains("was not changed"));
}

#[test]
fn selected_text_translation_has_no_recording_shortcuts() {
    let settings = Settings::default();
    assert!(voice_mode_shortcuts(&settings, PipelineMode::SelectedText).is_none());
    assert_eq!(
        voice_mode_shortcuts(&settings, PipelineMode::Ask),
        Some(&settings.shortcuts.ask)
    );
    assert_eq!(
        voice_mode_event_name(PipelineMode::SelectedText),
        SELECTED_TEXT_HISTORY_MODE
    );
}

#[tokio::test]
async fn cancel_clears_the_translation_target_only_when_it_cancels_a_recording() {
    let lifecycle = PipelineLifecycle::default();
    let audio = test_audio(false);
    let target = Mutex::new(None);
    let live = tokio::sync::Mutex::new(None);
    let edit = tokio::sync::Mutex::new(None);
    let ask = tokio::sync::Mutex::new(None);
    // Nothing to cancel: a target published by a concurrent start stays.
    let language = Mutex::new(Some("ja".to_string()));
    assert!(cancel_pipeline_operation(
        &lifecycle,
        &audio,
        &target,
        &language,
        &live,
        &edit,
        &ask,
        || ()
    )
    .await
    .unwrap()
    .is_none());
    assert_eq!(language.lock().unwrap().as_deref(), Some("ja"));

    // Processing owns its published target; cancellation only signals it.
    let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
    lifecycle.mark_recording(id).unwrap();
    lifecycle.begin_processing().unwrap();
    assert_eq!(
        cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &language,
            &live,
            &edit,
            &ask,
            || ()
        )
        .await
        .unwrap(),
        Some(PipelinePhase::Processing)
    );
    assert_eq!(language.lock().unwrap().as_deref(), Some("ja"));
    lifecycle.finish(id);

    let id = lifecycle.begin_start(PipelineMode::Translate).unwrap();
    lifecycle.mark_recording(id).unwrap();
    assert_eq!(
        cancel_pipeline_operation(
            &lifecycle,
            &audio,
            &target,
            &language,
            &live,
            &edit,
            &ask,
            || ()
        )
        .await
        .unwrap(),
        Some(PipelinePhase::Recording)
    );
    assert_eq!(*language.lock().unwrap(), None);
}

#[test]
fn correction_failure_status_excludes_provider_response_body() {
    for status in [
        reqwest::StatusCode::UNAUTHORIZED,
        reqwest::StatusCode::FORBIDDEN,
    ] {
        let error = correction::CorrectionError::Api {
            status,
            message: "synthetic-provider-secret".into(),
        };
        let message = correction_failure_status(&error);
        assert!(message.contains("rejected the API key"));
        assert!(message.contains("Using the original transcript."));
        assert!(!message.contains("synthetic-provider-secret"));
        assert!(edit_failure_message(&error).contains("The original selection was not changed."));
    }
    let invalid = correction::CorrectionError::InvalidApiKey {
        status: reqwest::StatusCode::BAD_REQUEST,
    };
    assert!(correction_failure_status(&invalid).contains("rejected the API key"));
    let error = correction::CorrectionError::Api {
        status: reqwest::StatusCode::BAD_REQUEST,
        message: "synthetic-provider-secret".into(),
    };
    let message = correction_failure_status(&error);
    assert!(message.contains("provider returned an error"));
    assert!(!message.contains("synthetic-provider-secret"));
}

#[test]
fn correction_failure_status_distinguishes_the_output_limit() {
    let local = correction_failure_status(&correction::CorrectionError::OutputLimit);
    let cloud = correction_failure_status(&correction::CorrectionError::ProviderOutputLimit);
    assert!(local.contains("local AI model"));
    assert!(cloud.contains("AI provider"));
    assert_ne!(local, cloud);
    for message in [&local, &cloud] {
        assert!(message.contains("output token limit"));
        assert!(message.contains("Using the original transcript."));
    }
    assert!(
        output_limit_inserted_message(&correction::CorrectionError::OutputLimit)
            .unwrap()
            .contains("the original transcript was inserted")
    );
    assert!(
        output_limit_inserted_message(&correction::CorrectionError::ProviderOutputLimit)
            .unwrap()
            .contains("the original transcript was inserted")
    );
    assert_eq!(
        output_limit_inserted_message(&correction::CorrectionError::Cancelled),
        None
    );
}

#[test]
fn protected_content_rejection_has_distinct_status_and_metric() {
    let error = correction::CorrectionError::ProtectedContentChanged(
        correction::ProtectedContentKind::Name,
    );
    assert!(correction_failure_status(&error).contains("protected"));
    assert_eq!(
        correction_failure_metric_code(&error),
        "protected_name_changed"
    );
    let invalid = correction::CorrectionError::InvalidResponse("synthetic-provider-secret".into());
    assert_eq!(
        correction_failure_metric_code(&invalid),
        "ai_invalid_response"
    );
    assert!(!correction_failure_status(&invalid).contains("synthetic-provider-secret"));
}
#[test]
fn a_failed_microphone_or_the_time_limit_stops_the_recording() {
    let limit = MAX_RECORDING_DURATION;
    assert_eq!(limit, Duration::from_secs(15 * 60));
    assert_eq!(
        automatic_stop_reason(false, Duration::from_secs(60), limit),
        None
    );
    assert_eq!(
        automatic_stop_reason(true, Duration::from_secs(1), limit),
        Some(StopReason::MicrophoneDisconnected)
    );
    assert_eq!(
        automatic_stop_reason(false, limit, limit),
        Some(StopReason::MaximumDuration)
    );
    assert_eq!(
        automatic_stop_reason(true, limit * 2, limit),
        Some(StopReason::MicrophoneDisconnected)
    );
}

#[test]
fn a_stream_failure_is_reported_even_when_the_user_stopped_first() {
    let artifact = |warning: Option<&str>| audio::AudioArtifact {
        path: PathBuf::from("take.wav"),
        sample_rate: 24_000,
        channels: 1,
        sample_count: 24_000,
        duration: Duration::from_secs(1),
        peak_level: 0.5,
        rms_level: 0.1,
        vad: audio::VadAnalysis::default(),
        warning: warning.map(str::to_owned),
    };
    assert!(!microphone_disconnected(
        StopReason::User,
        &Ok(artifact(None))
    ));
    assert!(microphone_disconnected(
        StopReason::User,
        &Ok(artifact(Some("device disconnected")))
    ));
    assert!(microphone_disconnected(
        StopReason::MicrophoneDisconnected,
        &Ok(artifact(None))
    ));
    assert!(microphone_disconnected(
        StopReason::User,
        &Err(audio::AudioError::StreamFailure(
            "device disconnected".into()
        ))
    ));
    assert!(!microphone_disconnected(
        StopReason::MaximumDuration,
        &Err(audio::AudioError::NoVoiceDetected)
    ));
}

#[test]
fn an_automatic_stop_keeps_its_reason_in_the_overlay() {
    assert_eq!(
        processing_message(StopReason::User, false, "Transcribing locally."),
        "Transcribing locally."
    );
    assert_eq!(
        processing_message(StopReason::User, true, "Transcribing locally."),
        MICROPHONE_DISCONNECTED_PROCESSING
    );
    assert_eq!(
        processing_message(StopReason::MicrophoneDisconnected, false, "Stopping"),
        MICROPHONE_DISCONNECTED_PROCESSING
    );
    assert_eq!(
        processing_message(StopReason::MaximumDuration, false, "Stopping"),
        RECORDING_LIMIT_PROCESSING
    );
    assert_eq!(
        processing_message(StopReason::MaximumDuration, true, "Stopping"),
        MICROPHONE_DISCONNECTED_PROCESSING
    );
}

#[test]
fn a_hotkey_start_failure_is_published_for_the_overlay() {
    let idle = Some(&AppPhase::Idle);
    let preparing = Some(&AppPhase::Processing);
    // A hotkey has no visible window: the failure must reach the overlay.
    assert_eq!(start_failure(false, true, idle), StartFailure::Error);
    assert_eq!(start_failure(false, true, None), StartFailure::Error);
    // The main window reports its own command error, unless the start
    // had already shown the preparing state.
    assert_eq!(start_failure(false, false, idle), StartFailure::Keep);
    assert_eq!(start_failure(false, false, preparing), StartFailure::Error);
    // A cancellation is not a failure; it only clears a busy phase.
    assert_eq!(start_failure(true, true, idle), StartFailure::Keep);
    assert_eq!(start_failure(true, true, preparing), StartFailure::Idle);
    assert_eq!(start_failure(true, false, preparing), StartFailure::Idle);
    assert_eq!(
        start_failure(true, false, Some(&AppPhase::Completed)),
        StartFailure::Keep
    );
}

#[test]
fn only_a_start_that_must_load_the_model_shows_the_preparing_state() {
    let status = |state: &str, model: &str| ModelStatus {
        model_id: Some(model.into()),
        quantization: "4bit".into(),
        installed: true,
        state: state.into(),
        detail: String::new(),
    };
    let current = Some("faster-whisper:large-v3-turbo".to_string());
    let ready = status("ready", "faster-whisper:large-v3-turbo");
    assert!(!needs_model_preparation(&ready, &current));
    for state in ["loading", "not_loaded", "error"] {
        assert!(needs_model_preparation(
            &status(state, "faster-whisper:large-v3-turbo"),
            &current
        ));
    }
    // A ready model that is not the configured one is reloaded first.
    assert!(needs_model_preparation(
        &status("ready", "faster-whisper:small"),
        &current
    ));
}

#[tokio::test]
async fn cancelling_while_the_model_loads_never_starts_the_recording() {
    let lifecycle = PipelineLifecycle::default();
    let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    let cancel = lifecycle.cancellation(id).unwrap();
    let (finish_load, load_finished) = tokio::sync::oneshot::channel::<()>();
    let waiting = load_unless_cancelled(
        async move {
            let _ = load_finished.await;
            Ok(())
        },
        cancel,
    );
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), waiting.as_mut())
            .await
            .is_err()
    );
    // A second press cancels the Starting operation mid-load.
    assert_eq!(
        lifecycle.cancel().unwrap(),
        Some((id, PipelinePhase::Starting))
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap(),
        Ok(true)
    );
    // The load finishing later cannot open the microphone.
    let _ = finish_load.send(());
    assert!(lifecycle.mark_recording(id).is_err());
    lifecycle.finish(id);
    assert!(lifecycle.begin_start(PipelineMode::Dictate).is_ok());
}

#[tokio::test]
async fn a_ready_model_starts_without_waiting_for_cancellation() {
    let lifecycle = PipelineLifecycle::default();
    let id = lifecycle.begin_start(PipelineMode::Dictate).unwrap();
    let cancel = lifecycle.cancellation(id).unwrap();
    assert_eq!(
        load_unless_cancelled(async { Ok(()) }, cancel.clone()).await,
        Ok(false)
    );
    assert_eq!(
        load_unless_cancelled(async { Err("load failed".into()) }, cancel).await,
        Err("load failed".to_string())
    );
}

#[test]
fn known_start_errors_become_fixed_translatable_messages() {
    let wrapped = |detail: &str| format!("local operation failed: {detail}");
    assert_eq!(
        start_failure_message(&wrapped("no foreground target is available")),
        START_NO_TARGET
    );
    assert_eq!(
        start_failure_message(&wrapped("text insertion into secure controls is disabled")),
        START_SECURE_TARGET
    );
    assert_eq!(
        start_failure_message(&wrapped(
            "the target cannot be accessed at this privilege level"
        )),
        START_ELEVATED_TARGET
    );
    // The device id is not part of the message.
    let missing = start_failure_message(&wrapped(
        "audio input device not found: {0.0.1.00000000}.{abc}",
    ));
    assert_eq!(missing, START_MICROPHONE_NOT_FOUND);
    assert!(!missing.contains("abc"));
    assert_eq!(
        start_failure_message(&wrapped("failed to build input stream: busy")),
        START_MICROPHONE_UNAVAILABLE
    );
    // Already fixed sentences and unknown errors pass through.
    assert_eq!(
        start_failure_message("Select text in a supported foreground edit control."),
        "Select text in a supported foreground edit control."
    );
    assert_eq!(
        start_failure_message(&wrapped("worker exited unexpectedly")),
        wrapped("worker exited unexpectedly")
    );
}

fn model_change() -> (Settings, Settings) {
    let previous = Settings::default();
    let mut next = previous.clone();
    next.model_id = Some("custom/model".into());
    (previous, next)
}

#[test]
fn a_recording_cannot_start_between_the_model_check_and_reconfigure() {
    let gate = ModelChangeGate::new();
    let lifecycle = PipelineLifecycle::default();
    let (previous, next) = model_change();

    // update_settings: the Idle check and the mark happen in one step.
    let change = gate
        .begin_change(&previous, &next, || lifecycle.phase())
        .unwrap();
    assert!(change.is_some());
    assert_eq!(
        gate.begin_pipeline(|| lifecycle.begin_start(PipelineMode::Dictate)),
        Err(MODEL_CHANGE_IN_PROGRESS)
    );
    assert_eq!(lifecycle.phase(), PipelinePhase::Idle);

    // After the reconfiguration the start proceeds, and a later model
    // change observes the claimed pipeline and is refused.
    drop(change);
    assert!(gate
        .begin_pipeline(|| lifecycle.begin_start(PipelineMode::Dictate))
        .is_ok());
    assert_eq!(
        gate.begin_change(&previous, &next, || lifecycle.phase())
            .err(),
        Some(MODEL_CHANGE_WHILE_BUSY.to_string())
    );
    // Unrelated settings never take the mark, even while recording.
    assert!(gate
        .begin_change(&previous, &previous, || lifecycle.phase())
        .unwrap()
        .is_none());
}

#[test]
fn manual_model_preparation_refuses_active_takes_and_prevents_queued_recordings() {
    let gate = ModelChangeGate::new();
    assert!(gate.begin_preparation(|| PipelinePhase::Recording).is_err());
    assert!(gate
        .begin_preparation(|| PipelinePhase::Processing)
        .is_err());
    let (preparation, cancel) = gate.begin_preparation(|| PipelinePhase::Idle).unwrap();
    assert!(!*cancel.borrow());
    assert_eq!(
        gate.begin_pipeline(|| Ok(())),
        Err(MODEL_CHANGE_IN_PROGRESS)
    );
    assert!(*cancel.borrow());
    assert!(gate.begin_preparation(|| PipelinePhase::Idle).is_err());
    drop(preparation);
    assert_eq!(gate.begin_pipeline(|| Ok(())), Ok(()));
}

#[test]
fn disabling_routing_cancels_preparation_and_can_be_retried_after_recovery() {
    let gate = ModelChangeGate::new();
    let (preparation, cancel) = gate.begin_preparation(|| PipelinePhase::Idle).unwrap();
    let previous = Settings {
        automatic_voice_mode: true,
        ..Settings::default()
    };
    let next = Settings::default();
    assert!(gate
        .begin_change(&previous, &next, || PipelinePhase::Idle)
        .is_err());
    assert!(*cancel.borrow());
    drop(preparation);
    assert!(gate
        .begin_change(&previous, &next, || PipelinePhase::Idle)
        .unwrap()
        .is_some());
}

#[test]
fn decision_settings_never_restart_the_speech_worker() {
    let previous = Settings::default();
    let mut next = previous.clone();
    next.automatic_voice_mode = true;
    next.decision_model = "Cloudflare/clef".into();
    next.decision_quantization = "8bit".into();
    assert!(!worker_configuration_changed(&previous, &next));
    assert!(speech_model_changed(&previous, &next));
    assert!(reject_model_change_while_busy(&previous, &next, PipelinePhase::Recording).is_err());
}

#[test]
fn overlapping_model_changes_are_refused() {
    let gate = ModelChangeGate::new();
    let (previous, next) = model_change();
    let first = gate
        .begin_change(&previous, &next, || PipelinePhase::Idle)
        .unwrap();
    assert!(first.is_some());
    // A stale save reverting the model while the first change is pending.
    assert_eq!(
        gate.begin_change(&next, &previous, || PipelinePhase::Idle)
            .err(),
        Some(MODEL_CHANGE_IN_PROGRESS.to_string())
    );
    // The refused change did not clear the first change's mark.
    assert_eq!(
        gate.begin_pipeline(|| Ok(())),
        Err(MODEL_CHANGE_IN_PROGRESS)
    );
    drop(first);
    assert_eq!(gate.begin_pipeline(|| Ok(())), Ok(()));
    assert!(gate
        .begin_change(&next, &previous, || PipelinePhase::Idle)
        .unwrap()
        .is_some());
}

#[test]
fn credential_changes_refuse_busy_operations_and_keep_starts_out_until_reconfigured() {
    let gate = ModelChangeGate::new();
    assert!(gate
        .begin_secret_change(|| PipelinePhase::Recording)
        .is_err());
    assert!(gate
        .begin_secret_change(|| PipelinePhase::Processing)
        .is_err());
    assert!(gate
        .begin_secret_change(|| PipelinePhase::Starting)
        .is_err());
    let change = gate.begin_secret_change(|| PipelinePhase::Idle).unwrap();
    assert_eq!(
        gate.begin_pipeline(|| Ok(())),
        Err(MODEL_CHANGE_IN_PROGRESS)
    );
    assert!(gate.begin_secret_change(|| PipelinePhase::Idle).is_err());
    drop(change);
    assert_eq!(gate.begin_pipeline(|| Ok(())), Ok(()));
}

#[test]
fn a_start_during_a_model_change_fails_fast_instead_of_waiting() {
    let gate = Arc::new(ModelChangeGate::new());
    let lifecycle = PipelineLifecycle::default();
    let (previous, next) = model_change();
    let (marked, wait_marked) = std::sync::mpsc::channel();
    let (release, wait_release) = std::sync::mpsc::channel::<()>();
    // A reconfiguration that is still running (for example behind a
    // model download that holds the transcriber).
    let updater = std::thread::spawn({
        let gate = Arc::clone(&gate);
        move || {
            let _change = gate
                .begin_change(&previous, &next, || PipelinePhase::Idle)
                .unwrap();
            marked.send(()).unwrap();
            wait_release.recv().unwrap();
        }
    });
    wait_marked.recv().unwrap();
    let started = Instant::now();
    assert_eq!(
        gate.begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::Dictate))
            .map(|(id, _)| id),
        Err(MODEL_CHANGE_IN_PROGRESS)
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(lifecycle.phase(), PipelinePhase::Idle);
    release.send(()).unwrap();
    updater.join().unwrap();
    // The guard cleared the mark when the update finished.
    assert!(gate
        .begin_pipeline(|| lifecycle.begin_non_recording(PipelineMode::Dictate))
        .is_ok());
}

#[test]
fn model_changes_wait_for_an_idle_pipeline() {
    let previous = Settings::default();
    let mut backend = previous.clone();
    backend.asr_backend = if previous.asr_backend == "mock" {
        "faster-whisper"
    } else {
        "mock"
    }
    .into();
    let mut model = previous.clone();
    model.model_id = Some("custom/model".into());
    let mut quantization = previous.clone();
    quantization.model_quantization = if previous.model_quantization == "4bit" {
        "8bit"
    } else {
        "4bit"
    }
    .into();
    assert_ne!(previous.asr_backend, "openai-compatible");
    let mut api = previous.clone();
    api.asr_backend = "openai-compatible".into();
    let mut endpoint = api.clone();
    endpoint.api_base_url = "https://asr.example.invalid/v1".into();
    // The API fields do not reach a local backend's worker.
    let mut local_endpoint = previous.clone();
    local_endpoint.api_base_url = "https://asr.example.invalid/v1".into();
    local_endpoint.api_key_env_var = "OTHER_ASR_API_KEY".into();
    let mut unrelated = previous.clone();
    unrelated.interaction_sounds = !previous.interaction_sounds;
    for busy in [
        PipelinePhase::Starting,
        PipelinePhase::Recording,
        PipelinePhase::Processing,
    ] {
        for (from, next) in [
            (&previous, &backend),
            (&previous, &model),
            (&previous, &quantization),
            (&api, &endpoint),
        ] {
            assert_eq!(
                reject_model_change_while_busy(from, next, busy),
                Err(MODEL_CHANGE_WHILE_BUSY.to_string())
            );
            assert_eq!(
                reject_model_change_while_busy(from, next, PipelinePhase::Idle),
                Ok(())
            );
        }
        for next in [&unrelated, &local_endpoint] {
            assert_eq!(
                reject_model_change_while_busy(&previous, next, busy),
                Ok(())
            );
        }
    }
}

#[test]
fn only_a_different_worker_command_counts_as_a_worker_change() {
    let local = Settings::default();
    assert_ne!(local.asr_backend, "openai-compatible");
    let mut api = local.clone();
    api.asr_backend = "openai-compatible".into();
    let changed = |edit: fn(&mut Settings), from: &Settings| {
        let mut next = from.clone();
        edit(&mut next);
        (from.clone(), next)
    };
    let unchanged = [
        changed(
            |next| next.api_base_url = "https://asr.example.invalid/v1".into(),
            &local,
        ),
        changed(
            |next| next.api_key_env_var = "OTHER_ASR_API_KEY".into(),
            &local,
        ),
        changed(|next| next.model_id = Some("  ".into()), &local),
        changed(
            |next| next.api_base_url = format!(" {} ", next.api_base_url),
            &api,
        ),
        changed(
            |next| next.interaction_sounds = !next.interaction_sounds,
            &api,
        ),
    ];
    for (previous, next) in &unchanged {
        assert!(!worker_configuration_changed(previous, next));
        assert!(!speech_model_changed(previous, next));
        // Skipping reconfigure is sound: the worker command is the same.
        assert_eq!(
            worker_command_for_settings(previous),
            worker_command_for_settings(next)
        );
    }
    let worker_changes = [
        changed(|next| next.model_quantization = "8bit".into(), &local),
        changed(|next| next.asr_backend = "openai-compatible".into(), &local),
        changed(|next| next.model_id = Some("custom/model".into()), &local),
        changed(
            |next| next.api_base_url = "https://asr.example.invalid/v1".into(),
            &api,
        ),
        changed(
            |next| next.api_key_env_var = "OTHER_ASR_API_KEY".into(),
            &api,
        ),
    ];
    for (previous, next) in &worker_changes {
        assert!(worker_configuration_changed(previous, next));
        assert!(speech_model_changed(previous, next));
    }
}

#[test]
fn device_policy_changes_invalidate_loaded_workers_and_wait_for_idle() {
    let previous = Settings::default();
    let mut next = previous.clone();
    next.asr_device = Some(types::AsrDevice::Cpu);
    assert!(worker_configuration_changed(&previous, &next));
    assert_ne!(
        worker_command_for_settings(&previous),
        worker_command_for_settings(&next)
    );
    for phase in [
        PipelinePhase::Starting,
        PipelinePhase::Recording,
        PipelinePhase::Processing,
    ] {
        assert!(reject_model_change_while_busy(&previous, &next, phase).is_err());
    }
    assert!(reject_model_change_while_busy(&previous, &next, PipelinePhase::Idle).is_ok());
    for backend in ["vibevoice", "openai-compatible"] {
        let mut previous = previous.clone();
        previous.asr_backend = backend.into();
        next = previous.clone();
        next.asr_device = Some(types::AsrDevice::Cpu);
        assert!(!worker_configuration_changed(&previous, &next));
        assert_eq!(
            worker_command_for_settings(&previous),
            worker_command_for_settings(&next)
        );
    }
}

fn failed_take_storage(settings: Settings) -> (Storage, tempfile::TempDir, PathBuf) {
    let storage = Storage::in_memory().unwrap();
    storage.update_settings(&settings).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let take = directory.path().join("failed-take.wav");
    std::fs::write(&take, b"RIFF failed take").unwrap();
    (storage, directory, take)
}

/// History and its audio are kept, so a failed take is a normal row.
fn keeping_audio() -> Settings {
    Settings {
        history_retention: types::HistoryRetention::OneMonth,
        delete_audio_after_processing: false,
        ..Settings::default()
    }
}

/// The most private settings: no History and no audio.
fn keeping_nothing(keep_failed_takes: bool) -> Settings {
    Settings {
        history_retention: types::HistoryRetention::Never,
        delete_audio_after_processing: true,
        keep_failed_takes,
        ..Settings::default()
    }
}

fn dictate_failure() -> Option<NewHistoryItem<'static>> {
    failed_take_history_item(PipelineMode::Dictate, "mock", None, None, None, 1)
}

fn history_audio_count(storage: &Storage) -> usize {
    storage
        .list_history(types::HistoryFilter::All, 10)
        .unwrap()
        .iter()
        .filter(|row| row.has_audio)
        .count()
}

#[test]
fn a_failed_transcription_is_kept_in_history_for_retry() {
    let (storage, _directory, take) = failed_take_storage(keeping_audio());
    let settings = storage.get_settings().unwrap();
    let item = failed_take_history_item(
        PipelineMode::Dictate,
        "faster-whisper:large-v3-turbo",
        None,
        None,
        Some("messaging"),
        90_000,
    );
    let kept = preserve_failed_take(&storage, &settings, item, &take);
    assert_eq!(kept, Some(storage::FailedTakeRetention::Normal));
    assert_eq!(
        failed_take_message(kept, TranscriptionFailure::Retryable),
        TRANSCRIPTION_FAILED_SAVED_TO_HISTORY
    );

    let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert!(row.has_audio);
    assert_eq!(row.expires_at, None, "normal History retention applies");
    assert_eq!(row.transcript_text, "");
    assert_eq!(
        row.insertion_result.as_deref(),
        Some(TRANSCRIPTION_FAILED_OUTCOME)
    );
    assert_eq!(row.duration_ms, Some(90_000));
    assert_eq!(history_pipeline_mode(&row.mode), Ok(PipelineMode::Dictate));
    let retry_copy = storage.copy_history_audio_for_retry(row.id).unwrap();
    assert_eq!(std::fs::read(&retry_copy).unwrap(), b"RIFF failed take");
    std::fs::remove_file(retry_copy).unwrap();
    // History keeps its own copy; the caller still removes its temporary take.
    assert!(take.is_file());
    storage.delete_all_history().unwrap();
}

#[test]
fn a_failed_take_is_kept_for_retry_even_when_history_and_audio_are_off() {
    for settings in [
        keeping_nothing(true),
        Settings {
            delete_audio_after_processing: false,
            ..keeping_nothing(true)
        },
        Settings {
            history_retention: types::HistoryRetention::OneMonth,
            ..keeping_nothing(true)
        },
    ] {
        let (storage, _directory, take) = failed_take_storage(settings.clone());
        let kept = preserve_failed_take(&storage, &settings, dictate_failure(), &take);
        assert_eq!(kept, Some(storage::FailedTakeRetention::Temporary));
        assert_eq!(
            failed_take_message(kept, TranscriptionFailure::Retryable),
            TRANSCRIPTION_FAILED_KEPT_FOR_RETRY
        );

        let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 1, "{settings:?}");
        let row = &rows[0];
        assert!(row.has_audio);
        assert!(row.expires_at.is_some());
        let retry_copy = storage.copy_history_audio_for_retry(row.id).unwrap();
        assert_eq!(std::fs::read(&retry_copy).unwrap(), b"RIFF failed take");
        std::fs::remove_file(retry_copy).unwrap();
        storage.delete_all_history().unwrap();
    }
}

#[test]
fn without_keeping_failed_takes_only_history_with_audio_keeps_them() {
    for settings in [
        keeping_nothing(false),
        Settings {
            delete_audio_after_processing: false,
            ..keeping_nothing(false)
        },
        Settings {
            history_retention: types::HistoryRetention::OneMonth,
            ..keeping_nothing(false)
        },
    ] {
        let (storage, _directory, take) = failed_take_storage(settings.clone());
        let kept = preserve_failed_take(&storage, &settings, dictate_failure(), &take);
        assert_eq!(kept, None, "{settings:?}");
        assert_eq!(
            failed_take_message(kept, TranscriptionFailure::Retryable),
            "Transcription failed. Check the selected model and Diagnostics."
        );
        assert!(storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
    }

    let settings = Settings {
        keep_failed_takes: false,
        ..keeping_audio()
    };
    let (storage, _directory, take) = failed_take_storage(settings.clone());
    assert_eq!(
        preserve_failed_take(&storage, &settings, dictate_failure(), &take),
        Some(storage::FailedTakeRetention::Normal)
    );
    storage.delete_all_history().unwrap();
}

#[test]
fn a_failed_take_is_never_saved_without_its_audio() {
    // Audio retention and keeping failed takes were turned off while the
    // take was being transcribed.
    let (storage, _directory, take) = failed_take_storage(Settings {
        history_retention: types::HistoryRetention::OneMonth,
        ..keeping_nothing(false)
    });
    let snapshot = keeping_audio();
    assert_eq!(
        preserve_failed_take(&storage, &snapshot, dictate_failure(), &take),
        None
    );
    assert!(storage
        .list_history(types::HistoryFilter::All, 10)
        .unwrap()
        .is_empty());

    // The audio copy fails (here: the take no longer exists).
    for settings in [keeping_audio(), keeping_nothing(true)] {
        let (storage, directory, _take) = failed_take_storage(settings.clone());
        let missing = directory.path().join("missing.wav");
        assert_eq!(
            preserve_failed_take(&storage, &settings, dictate_failure(), &missing),
            None
        );
        assert!(storage
            .list_history(types::HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
    }
}

fn retry_result(source_id: i64) -> NewHistoryItem<'static> {
    NewHistoryItem {
        transcript_text: "retried",
        processed_text: None,
        source_text: None,
        instruction_text: None,
        action_kind: None,
        search_site: None,
        mode: "faithful",
        asr_provider: "mock",
        llm_provider: None,
        target_language: None,
        app_category: None,
        duration_ms: Some(1),
        latency_ms: Some(1),
        retry_of_id: Some(source_id),
        insertion_result: None,
        insertion_detail: None,
    }
}

#[test]
fn a_successful_retry_applies_the_normal_settings_to_a_failed_take() {
    // History off: neither the failed take nor the Retry result remains.
    let (storage, directory, take) = failed_take_storage(keeping_nothing(true));
    let settings = storage.get_settings().unwrap();
    preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
    let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
    let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
    assert_eq!(
        save_retry_result(&storage, &retry_result(source), &retry_audio, source),
        HistorySaveStatus::Complete
    );
    std::fs::remove_file(retry_audio).unwrap();
    assert!(storage.history_item(source).unwrap().is_none());
    assert!(storage
        .list_history(types::HistoryFilter::All, 10)
        .unwrap()
        .is_empty());
    drop(directory);

    // History on, audio deleted: the Retry text is kept without audio.
    let (storage, _directory, take) = failed_take_storage(Settings {
        history_retention: types::HistoryRetention::OneMonth,
        ..keeping_nothing(true)
    });
    let settings = storage.get_settings().unwrap();
    preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
    let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
    let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
    save_retry_result(&storage, &retry_result(source), &retry_audio, source);
    std::fs::remove_file(retry_audio).unwrap();
    assert!(storage.history_item(source).unwrap().is_none());
    let rows = storage.list_history(types::HistoryFilter::All, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].transcript_text, "retried");
    assert_eq!(history_audio_count(&storage), 0);

    // History with audio enabled since the failure: the take stays as a
    // normal row instead of expiring.
    let (storage, _directory, take) = failed_take_storage(keeping_nothing(true));
    let settings = storage.get_settings().unwrap();
    preserve_failed_take(&storage, &settings, dictate_failure(), &take).unwrap();
    let source = storage.list_history(types::HistoryFilter::All, 1).unwrap()[0].id;
    storage
        .update_settings_and_apply_history_policy(&keeping_audio())
        .unwrap();
    let retry_audio = storage.copy_history_audio_for_retry(source).unwrap();
    save_retry_result(&storage, &retry_result(source), &retry_audio, source);
    std::fs::remove_file(retry_audio).unwrap();
    let kept = storage.history_item(source).unwrap().unwrap();
    assert!(kept.has_audio);
    assert_eq!(kept.expires_at, None);
    assert_eq!(history_audio_count(&storage), 2);
    storage.delete_all_history().unwrap();
}

#[test]
fn a_truncated_transcript_is_not_presented_as_retryable() {
    let worker = |code: &str| {
        asr::AsrError::Worker(asr::WorkerError {
            code: code.into(),
            message: "failed".into(),
        })
    };
    assert_eq!(
        TranscriptionFailure::of(&worker(TRANSCRIPT_TRUNCATED_CODE)),
        TranscriptionFailure::TooLongForModel
    );
    for error in [
        worker("transcription_failed"),
        worker("cuda_oom"),
        asr::AsrError::Timeout,
        asr::AsrError::Crashed,
        asr::AsrError::Protocol("transcript_truncated".into()),
    ] {
        assert_eq!(
            TranscriptionFailure::of(&error),
            TranscriptionFailure::Retryable,
            "{error}"
        );
    }

    let too_long = TranscriptionFailure::TooLongForModel;
    assert_eq!(too_long.history_detail(), Some("transcript_truncated"));
    assert_eq!(TranscriptionFailure::Retryable.history_detail(), None);
    assert_eq!(
        failed_take_message(None, too_long),
        TRANSCRIPTION_TOO_LONG_FOR_MODEL
    );
    for kept in [
        storage::FailedTakeRetention::Normal,
        storage::FailedTakeRetention::Temporary,
    ] {
        assert_eq!(
            failed_take_message(Some(kept), too_long),
            TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY
        );
    }
    for message in [
        TRANSCRIPTION_TOO_LONG_FOR_MODEL,
        TRANSCRIPTION_TOO_LONG_SAVED_TO_HISTORY,
    ] {
        assert!(message.contains("will fail the same way"));
        assert!(message.contains("faster-whisper"));
    }
}

#[test]
fn cpu_memory_failures_offer_system_memory_guidance_with_truthful_retention() {
    let failure = TranscriptionFailure::of(&asr::AsrError::Worker(asr::WorkerError {
        code: "cpu_oom".into(),
        message: "private worker details".into(),
    }));
    assert_eq!(failure, TranscriptionFailure::CpuMemory);
    for kept in [
        None,
        Some(storage::FailedTakeRetention::Normal),
        Some(storage::FailedTakeRetention::Temporary),
    ] {
        let message = failed_take_message(kept, failure);
        assert!(message.contains("system memory"));
        assert!(!message.contains("GPU"));
        assert!(!message.contains("private"));
        assert!(crate::messages::code(message).is_some());
        assert_eq!(
            message.contains("24 hours"),
            kept == Some(storage::FailedTakeRetention::Temporary)
        );
    }
}

#[test]
fn a_truncated_failed_take_records_why_in_history() {
    let (storage, _directory, take) = failed_take_storage(keeping_nothing(true));
    let settings = storage.get_settings().unwrap();
    let item = dictate_failure().map(|item| NewHistoryItem {
        insertion_detail: TranscriptionFailure::TooLongForModel.history_detail(),
        ..item
    });
    assert!(preserve_failed_take(&storage, &settings, item, &take).is_some());
    let row = &storage.list_history(types::HistoryFilter::All, 1).unwrap()[0];
    assert_eq!(
        row.insertion_result.as_deref(),
        Some(TRANSCRIPTION_FAILED_OUTCOME)
    );
    assert_eq!(
        row.insertion_detail.as_deref(),
        Some("transcript_truncated")
    );
    storage.delete_all_history().unwrap();
}

#[test]
fn failed_takes_keep_what_retry_needs_for_each_mode() {
    let translate = failed_take_history_item(
        PipelineMode::Translate,
        "mock",
        None,
        Some("English"),
        None,
        1,
    )
    .unwrap();
    assert_eq!(translate.mode, "translate");
    assert_eq!(translate.target_language, Some("English"));
    let edit = failed_take_history_item(
        PipelineMode::Edit,
        "mock",
        Some("selected"),
        None,
        Some("messaging"),
        1,
    )
    .unwrap();
    assert_eq!(edit.mode, "edit");
    assert_eq!(edit.source_text, Some("selected"));
    assert_eq!(edit.app_category, None);
    for mode in ["faithful", "translate", "edit"] {
        assert!(history_pipeline_mode(mode).is_ok());
    }
    // Retry could not reproduce these takes, so they are not kept.
    assert!(
        failed_take_history_item(PipelineMode::Translate, "mock", None, None, None, 1).is_none()
    );
    assert!(failed_take_history_item(PipelineMode::Edit, "mock", None, None, None, 1).is_none());
    assert!(
        failed_take_history_item(PipelineMode::Ask, "mock", Some("selected"), None, None, 1)
            .is_none()
    );
}

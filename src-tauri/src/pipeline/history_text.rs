//! Explicit text-only History actions never open audio, load ASR or insert text.
use crate::{
    commands::{begin_history_retry, retry_app_context, retry_dictate_history_mode},
    correction::{self, FailureOutcome},
    dictionary::CorrectionTerm,
    emit_ai_failure, emit_stage, emit_state, personalization,
    state::{AppState, PipelineMode, TerminalPhaseGuard},
    storage::Storage,
    types::{self, AppPhase, AppStage, HistoryItem, NewHistoryItem, RecordingResult, Settings},
    PipelineGuard, Services,
};
use serde::Deserialize;
use std::time::Instant;
use tauri::{AppHandle, Emitter, Manager};

#[derive(Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Action {
    Correct { profile: Option<String> },
    Translate { target: String },
}

impl Action {
    fn mode(&self) -> PipelineMode {
        match self {
            Self::Correct { .. } => PipelineMode::Dictate,
            Self::Translate { .. } => PipelineMode::Translate,
        }
    }
}

struct Prepared {
    settings: Settings,
    source: HistoryItem,
    action: Action,
    hints: Vec<CorrectionTerm>,
    guidance: Option<String>,
}

fn prepare(storage: &Storage, id: i64, action: Action) -> Result<Prepared, String> {
    storage
        .enforce_current_history_policy()
        .map_err(crate::command_error)?;
    let source = storage
        .history_item(id)
        .map_err(crate::command_error)?
        .ok_or("history item not found")?;
    if source.transcript_text.trim().is_empty() {
        return Err("The saved transcript is empty.".into());
    }
    let settings = storage.get_settings().map_err(crate::command_error)?;
    crate::privacy::check_text(&settings).map_err(str::to_owned)?;
    let context = retry_app_context(&source);
    let profile = match &action {
        Action::Correct { profile: None } => {
            personalization::resolve_profile(&settings, context.as_ref())
        }
        Action::Correct {
            profile: Some(scope),
        } if scope == "global" => Some(
            settings
                .global_style_profile
                .clone()
                .ok_or("style profile is unavailable")?,
        ),
        Action::Correct {
            profile: Some(scope),
        } => Some(
            settings
                .scoped_style_profiles
                .iter()
                .find(|entry| &entry.scope == scope)
                .ok_or("style profile is unavailable")?
                .profile
                .clone(),
        ),
        Action::Translate { target } => {
            if !types::TRANSLATION_TARGET_LANGUAGES.contains(&target.as_str()) {
                return Err("unsupported translation target language".into());
            }
            None
        }
    };
    let hints = storage
        .dictionary_correction_terms(&source.transcript_text, context.as_ref())
        .map_err(crate::command_error)?;
    Ok(Prepared {
        settings,
        source,
        action,
        hints,
        guidance: profile.map(|profile| personalization::guidance(&profile)),
    })
}

/// The owned command task keeps lifecycle ownership even if its IPC caller leaves.
pub(crate) async fn run(
    app: AppHandle,
    id: i64,
    action: Action,
) -> Result<RecordingResult, String> {
    let services = app.state::<Services>();
    let state = app.state::<AppState>();
    let storage = app.state::<Storage>().inner().clone();
    let (operation_id, cancel) = begin_history_retry(&services, action.mode())?;
    let _owner = PipelineGuard {
        lifecycle: &services.lifecycle,
        id: operation_id,
    };
    let _terminal = TerminalPhaseGuard::new(
        &state,
        &services.lifecycle,
        operation_id,
        "History retry cancelled.",
        |phase, message| emit_state(&app, &state, phase, message),
    );
    state.set_operation(types::OperationKind::Retry, operation_id);
    crate::recording_overlay::anchor_to_foreground();
    let target = match &action {
        Action::Translate { target } => Some(target.as_str()),
        _ => None,
    };
    let _ = app.emit("voice-mode", serde_json::json!({ "mode": if target.is_some() { "translate" } else { "dictate" }, "targetLanguage": target }));
    if target.is_some() {
        emit_stage(
            &app,
            &state,
            AppStage::Translating,
            "Translating speech to the selected language.",
        );
    } else {
        emit_stage(
            &app,
            &state,
            AppStage::Correcting,
            "Correcting the transcript with the configured AI provider.",
        );
    }
    let started = Instant::now();
    let prepare_app = app.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let prepared = prepare(&storage, id, action)?;
        // This is an explicitly requested AI action, even when optional
        // correction of new dictation is disabled.
        correction::preflight_recording(&prepared.settings, PipelineMode::SelectedText).map_err(
            |error| {
                crate::commands::ai_command_error(&prepare_app, &error, FailureOutcome::NoAction)
            },
        )?;
        Ok::<_, String>(prepared)
    })
    .await
    .map_err(|_| "History retry failed.".to_owned())??;
    if services.lifecycle.is_cancelled(operation_id) {
        return Err("History retry cancelled.".into());
    }
    let result = match &prepared.action {
        Action::Correct { .. } => {
            emit_stage(
                &app,
                &state,
                AppStage::Correcting,
                "Correcting the transcript with the configured AI provider.",
            );
            correction::correct_transcript(
                &prepared.settings,
                &prepared.source.transcript_text,
                &prepared.hints,
                prepared.guidance.as_deref(),
                cancel,
                |_| {},
            )
            .await
        }
        Action::Translate { target } => {
            emit_stage(
                &app,
                &state,
                AppStage::Translating,
                "Translating speech to the selected language.",
            );
            correction::translate_transcript(
                &prepared.settings,
                &prepared.source.transcript_text,
                target,
                cancel,
                |_| {},
            )
            .await
        }
    };
    let (text, failure) = match super::classify_ai_stage(result) {
        super::AiStageResult::Corrected(text) => (text, None),
        super::AiStageResult::Cancelled => return Err("History retry cancelled.".into()),
        super::AiStageResult::Fallback(error) => {
            let event = if matches!(prepared.action, Action::Correct { .. }) {
                "text_correction_failed"
            } else {
                "voice_translation_failed"
            };
            emit_ai_failure(
                &app,
                event,
                &error,
                FailureOutcome::UsingTranscript,
                &correction::failure_message(&error, FailureOutcome::UsingTranscript),
            );
            (
                prepared.source.transcript_text.clone(),
                Some(error.failure_code()),
            )
        }
    };
    let latency = started.elapsed().as_millis() as i64;
    let commit_app = app.clone();
    let output = text.clone();
    let status = tokio::task::spawn_blocking(move || {
        let services = commit_app.state::<Services>();
        let storage = commit_app.state::<Storage>();
        services
            .lifecycle
            .commit_side_effect(operation_id, || {
                save(&storage, &prepared, &output, failure, latency)
            })
            .map_err(str::to_owned)?
    })
    .await
    .map_err(|_| "History retry failed.".to_owned())??;
    // Publication happens before releasing the lifecycle to the next operation.
    crate::recording_overlay::set_phase(&app, &AppPhase::Completed);
    let snapshot = state.complete(
        text.clone(),
        "History retry completed without inserting text.".into(),
    );
    let _ = app.emit("app-state", snapshot);
    super::finish::report_history_save_warning(&app, status);
    Ok(RecordingResult {
        text,
        insertion: "history_only".into(),
        duration_ms: 0,
        latency_ms: latency as u64,
    })
}

fn save(
    storage: &Storage,
    prepared: &Prepared,
    output: &str,
    failure: Option<&str>,
    latency: i64,
) -> Result<super::finish::HistorySaveStatus, String> {
    let target = match &prepared.action {
        Action::Translate { target } => Some(target.as_str()),
        _ => None,
    };
    let provider = failure
        .is_none()
        .then_some(prepared.settings.correction_provider.as_str());
    let event = if target.is_some() {
        "voice_translation"
    } else {
        "text_correction"
    };
    let result = storage.save_history_text_retry(
        prepared.source.id,
        &NewHistoryItem {
            transcript_text: &prepared.source.transcript_text,
            processed_text: (output != prepared.source.transcript_text).then_some(output),
            source_text: None,
            instruction_text: None,
            action_kind: None,
            search_site: None,
            mode: if target.is_some() {
                "translate"
            } else {
                retry_dictate_history_mode(provider, failure.is_some())
            },
            asr_provider: &prepared.source.asr_provider,
            llm_provider: provider,
            target_language: target,
            app_category: prepared.source.app_category.as_deref(),
            duration_ms: None,
            latency_ms: Some(latency),
            retry_of_id: Some(prepared.source.id),
            insertion_result: None,
            insertion_detail: None,
        },
    );
    let status = match result {
        Ok(None) => return Err("history item not found".into()),
        Ok(Some(_)) => super::finish::HistorySaveStatus::Complete,
        Err(_) => super::finish::HistorySaveStatus::Failed,
    };
    let _ = storage.add_metric(
        event,
        Some(&prepared.settings.correction_provider),
        Some(latency),
        failure.is_none(),
        failure,
    );
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PipelineLifecycle;
    use crate::types::{HistoryFilter, HistoryRetention, ScopedStyleProfile, StyleProfile};

    fn fixture() -> (Storage, i64) {
        fixture_with(Storage::in_memory().unwrap())
    }

    fn fixture_with(storage: Storage) -> (Storage, i64) {
        storage
            .update_settings(&Settings {
                correction_provider: "local".into(),
                history_retention: HistoryRetention::Forever,
                ..Settings::default()
            })
            .unwrap();
        storage
            .add_history(&NewHistoryItem {
                transcript_text: "raw transcript",
                processed_text: Some("previous processed answer"),
                source_text: Some("previous selection"),
                instruction_text: Some("previous instruction"),
                action_kind: Some("answer"),
                search_site: None,
                mode: "ask",
                asr_provider: "original-asr",
                llm_provider: Some("old-provider"),
                target_language: None,
                app_category: Some("email"),
                duration_ms: Some(1200),
                latency_ms: None,
                retry_of_id: None,
                insertion_result: Some("clipboard_paste"),
                insertion_detail: None,
            })
            .unwrap();
        let id = storage.list_history(HistoryFilter::All, 1).unwrap()[0].id;
        (storage, id)
    }

    #[test]
    fn text_reprocess_uses_raw_without_audio_and_saves_a_separate_history_only_result() {
        let (storage, id) = fixture();
        let prepared = prepare(
            &storage,
            id,
            Action::Translate {
                target: "ja".into(),
            },
        )
        .unwrap();
        assert_eq!(prepared.source.transcript_text, "raw transcript");
        assert!(!prepared.source.has_audio);
        assert_eq!(
            save(&storage, &prepared, "翻訳結果", None, 42).unwrap(),
            super::super::finish::HistorySaveStatus::Complete
        );
        let rows = storage.list_history(HistoryFilter::All, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let result = rows.iter().find(|row| row.retry_of_id == Some(id)).unwrap();
        assert_eq!(result.transcript_text, "raw transcript");
        assert_eq!(result.processed_text.as_deref(), Some("翻訳結果"));
        assert_eq!(result.target_language.as_deref(), Some("ja"));
        assert_eq!(result.asr_provider, "original-asr");
        assert!(!result.has_audio);
        assert!(
            result.source_text.is_none()
                && result.instruction_text.is_none()
                && result.insertion_result.is_none()
        );
        assert_eq!(
            storage
                .history_item(id)
                .unwrap()
                .unwrap()
                .processed_text
                .as_deref(),
            Some("previous processed answer")
        );
    }

    #[test]
    fn text_reprocess_requires_consent_and_rejects_unknown_target_and_profile() {
        let (storage, id) = fixture();
        assert!(prepare(
            &storage,
            id,
            Action::Translate {
                target: "invalid".into()
            }
        )
        .is_err());
        assert!(prepare(
            &storage,
            id,
            Action::Correct {
                profile: Some("app:missing".into())
            }
        )
        .is_err());
        let mut settings = storage.get_settings().unwrap();
        settings.correction_provider = "openai".into();
        settings.cloud_consents.clear();
        storage.update_settings(&settings).unwrap();
        assert_eq!(
            prepare(&storage, id, Action::Correct { profile: None })
                .err()
                .unwrap(),
            crate::privacy::TEXT_CONSENT_REQUIRED
        );
    }

    #[test]
    fn text_reprocess_selects_current_category_or_an_explicit_saved_profile() {
        let (storage, id) = fixture();
        let mut settings = storage.get_settings().unwrap();
        settings.personalization_enabled = true;
        settings.global_style_profile = Some(StyleProfile {
            formality: "formal".into(),
            detail: "concise".into(),
            guidance: Some("global style".into()),
        });
        settings.scoped_style_profiles = vec![ScopedStyleProfile {
            scope: "category:email".into(),
            profile: StyleProfile {
                formality: "casual".into(),
                detail: "detailed".into(),
                guidance: Some("email style".into()),
            },
        }];
        storage.update_settings(&settings).unwrap();
        let current = prepare(&storage, id, Action::Correct { profile: None }).unwrap();
        assert!(current.guidance.unwrap().contains("email style"));
        let global = prepare(
            &storage,
            id,
            Action::Correct {
                profile: Some("global".into()),
            },
        )
        .unwrap();
        assert!(global.guidance.unwrap().contains("global style"));
    }

    #[test]
    fn text_reprocess_does_not_resurrect_a_deleted_source() {
        let (storage, id) = fixture();
        let prepared = prepare(&storage, id, Action::Correct { profile: None }).unwrap();
        storage.delete_all_history().unwrap();
        assert!(save(&storage, &prepared, "new result", None, 1).is_err());
        assert!(storage
            .list_history(HistoryFilter::All, 10)
            .unwrap()
            .is_empty());
        assert!(prepare(&storage, id, Action::Correct { profile: None }).is_err());
    }

    #[test]
    fn text_reprocess_save_failure_keeps_output_available_with_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.sqlite3");
        let (storage, id) = fixture_with(Storage::open(&path).unwrap());
        let prepared = prepare(&storage, id, Action::Correct { profile: None }).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TRIGGER reject_history_insert BEFORE INSERT ON dictation_history BEGIN SELECT RAISE(ABORT, 'simulated storage failure'); END;").unwrap();
        let lifecycle = PipelineLifecycle::default();
        let (operation, _) = lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .unwrap();
        let status = lifecycle
            .commit_side_effect(operation, || {
                save(&storage, &prepared, "generated answer", None, 1)
            })
            .unwrap()
            .unwrap();
        assert_eq!(status, super::super::finish::HistorySaveStatus::Failed);
        assert_eq!(
            storage.list_history(HistoryFilter::All, 10).unwrap().len(),
            1
        );
    }

    #[test]
    fn cancelled_text_reprocess_cannot_commit_and_failure_retains_raw() {
        let (storage, id) = fixture();
        let prepared = prepare(&storage, id, Action::Correct { profile: None }).unwrap();
        let lifecycle = PipelineLifecycle::default();
        let (operation, _) = lifecycle
            .begin_non_recording(PipelineMode::Dictate)
            .unwrap();
        let _ = lifecycle.cancel();
        assert!(lifecycle
            .commit_side_effect(operation, || save(
                &storage,
                &prepared,
                "cancelled output",
                None,
                1
            ))
            .is_err());
        assert_eq!(
            storage.list_history(HistoryFilter::All, 10).unwrap().len(),
            1
        );
        save(
            &storage,
            &prepared,
            &prepared.source.transcript_text,
            Some("ai_unreachable"),
            1,
        )
        .unwrap();
        let rows = storage.list_history(HistoryFilter::All, 10).unwrap();
        let fallback = rows.iter().find(|row| row.retry_of_id == Some(id)).unwrap();
        assert_eq!(fallback.mode, "faithful_fallback");
        assert!(fallback.processed_text.is_none() && fallback.llm_provider.is_none());
    }
}

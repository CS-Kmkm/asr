//! Offline correction evaluation over the fixed text dataset (Issue #85).
//!
//! `scripts/eval_correction.py run --provider app` drives this through
//! `cargo run --example correction_eval`. Each case's transcript goes through
//! the same prompt, provider transport and meaning guard as Dictate; the
//! JSONL row records the provider's candidate, whether the guard accepted it,
//! and the text the app would insert (the candidate, or the raw transcript on
//! any failure). The candidate is untrusted text and is only recorded.
use super::{fact_check::accept_provider_correction, request_correction_candidate};
use crate::{dictionary::CorrectionTerm, types::Settings};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

/// One dataset case with the settings and dictionary terms it runs under.
pub(crate) struct EvalCase {
    pub(crate) id: String,
    pub(crate) transcript: String,
    pub(crate) settings: Settings,
    pub(crate) terms: Vec<CorrectionTerm>,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("case field {key} must be a string"))
}

/// Applies a camelCase settings patch over `base`, as stored settings are.
pub(crate) fn patched_settings(
    base: &Settings,
    patch: &Map<String, Value>,
) -> Result<Settings, String> {
    let mut value = serde_json::to_value(base).map_err(|error| error.to_string())?;
    let object = value.as_object_mut().ok_or("settings must be an object")?;
    for (key, item) in patch {
        if !object.contains_key(key) {
            return Err(format!("unknown setting {key}"));
        }
        object.insert(key.clone(), item.clone());
    }
    serde_json::from_value(value).map_err(|error| error.to_string())
}

pub(crate) fn parse_cases(dataset: &Value, base: &Settings) -> Result<Vec<EvalCase>, String> {
    if dataset.get("schema").and_then(Value::as_str) != Some("correction-eval/1") {
        return Err("dataset schema must be correction-eval/1".into());
    }
    let cases = dataset
        .get("cases")
        .and_then(Value::as_array)
        .ok_or("dataset cases must be a list")?;
    cases
        .iter()
        .map(|case| {
            let mut patch = case
                .get("settings")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            patch.insert(
                "correctionMode".into(),
                Value::String(text(case, "correction_mode")?.to_owned()),
            );
            patch.insert("textCorrectionEnabled".into(), Value::Bool(true));
            let terms = case
                .get("dictionary")
                .and_then(Value::as_array)
                .map(|terms| {
                    terms
                        .iter()
                        .map(|term| {
                            Ok(CorrectionTerm {
                                surface: text(term, "surface")?.to_owned(),
                                readings: term
                                    .get("readings")
                                    .and_then(Value::as_array)
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect(),
                            })
                        })
                        .collect::<Result<Vec<_>, String>>()
                })
                .transpose()?
                .unwrap_or_default();
            Ok(EvalCase {
                id: text(case, "id")?.to_owned(),
                transcript: text(case, "transcript")?.to_owned(),
                settings: patched_settings(base, &patch)?,
                terms,
            })
        })
        .collect()
}

/// The guard verdict for an already generated candidate.
#[cfg(test)]
fn guard_accepts(case: &EvalCase, candidate: &str) -> Result<(), &'static str> {
    accept_provider_correction(
        &case.settings,
        &case.transcript,
        &case.terms,
        candidate.to_owned(),
    )
    .map(|_| ())
    .map_err(|error| crate::commands::correction_failure_metric_code(&error))
}

async fn evaluate(case: &EvalCase) -> Value {
    let (_cancel_sender, cancel) = tokio::sync::watch::channel(false);
    let candidate = request_correction_candidate(
        &case.settings,
        &case.transcript,
        &case.terms,
        None,
        cancel,
        |_| {},
    )
    .await;
    match candidate {
        Ok((candidate, terms)) => {
            let verdict = accept_provider_correction(
                &case.settings,
                &case.transcript,
                &terms,
                candidate.clone(),
            );
            let accepted = verdict.is_ok();
            json!({
                "id": case.id,
                "candidate": candidate,
                "accepted": accepted,
                "error": verdict.err().map(|error| crate::commands::correction_failure_metric_code(&error)),
                "output": if accepted { candidate } else { case.transcript.clone() },
            })
        }
        Err(error) => json!({
            "id": case.id,
            "candidate": null,
            "accepted": false,
            "error": crate::commands::correction_failure_metric_code(&error),
            "output": case.transcript,
        }),
    }
}

struct Arguments {
    dataset: PathBuf,
    out: PathBuf,
    settings: Option<PathBuf>,
    only: Vec<String>,
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    let mut dataset = None;
    let mut out = None;
    let mut settings = None;
    let mut only = Vec::new();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--dataset" => dataset = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--settings" => settings = Some(PathBuf::from(value()?)),
            "--case" => only.push(value()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Arguments {
        dataset: dataset.ok_or("--dataset is required")?,
        out: out.ok_or("--out is required")?,
        settings,
        only,
    })
}

fn run(args: &[String]) -> Result<usize, String> {
    let arguments = parse_arguments(args)?;
    let read = |path: &PathBuf| -> Result<Value, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    };
    let base = match &arguments.settings {
        Some(path) => patched_settings(
            &Settings::default(),
            read(path)?
                .as_object()
                .ok_or("settings patch must be an object")?,
        )?,
        None => Settings::default(),
    };
    base.validate()?;
    let cases = parse_cases(&read(&arguments.dataset)?, &base)?
        .into_iter()
        .filter(|case| arguments.only.is_empty() || arguments.only.contains(&case.id))
        .collect::<Vec<_>>();
    let mut lines = String::new();
    tauri::async_runtime::block_on(async {
        for case in &cases {
            let row = evaluate(case).await;
            eprintln!(
                "{}: {}",
                case.id,
                if row["accepted"] == Value::Bool(true) {
                    "accepted"
                } else {
                    row["error"].as_str().unwrap_or("rejected")
                }
            );
            lines.push_str(&row.to_string());
            lines.push('\n');
        }
    });
    std::fs::write(&arguments.out, lines)
        .map_err(|error| format!("{}: {error}", arguments.out.display()))?;
    Ok(cases.len())
}

/// Entry point for `examples/correction_eval.rs`; returns the exit code.
pub(crate) fn run_cli(args: Vec<String>) -> i32 {
    match run(&args) {
        Ok(count) => {
            eprintln!("evaluated {count} cases");
            0
        }
        Err(error) => {
            eprintln!("correction_eval: {error}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATASET: &str =
        include_str!("../../../docs/eval_set/correction/correction-offline-v1.json");

    /// Every fixture with a deterministic guard expectation must get that
    /// verdict from the production guard under its case's settings; the
    /// reference rewrite of every case must be accepted.
    #[test]
    fn dataset_fixtures_get_their_expected_guard_verdicts() {
        let dataset: Value = serde_json::from_str(DATASET).unwrap();
        let cases = parse_cases(&dataset, &Settings::default()).unwrap();
        let mut mismatches = Vec::new();
        let mut caught = 0;
        let mut expected_rejections = 0;
        for (case, raw) in cases.iter().zip(dataset["cases"].as_array().unwrap()) {
            let reference = raw["reference"].as_str().unwrap();
            if let Err(code) = guard_accepts(case, reference) {
                mismatches.push(format!("{}/reference rejected: {code}", case.id));
            }
            for fixture in raw["fixtures"].as_array().unwrap() {
                let id = fixture["id"].as_str().unwrap();
                let verdict = guard_accepts(case, fixture["output"].as_str().unwrap());
                match fixture["guard"].as_str().unwrap() {
                    "accept" if verdict.is_err() => mismatches.push(format!(
                        "{}/{id} rejected: {}",
                        case.id,
                        verdict.unwrap_err()
                    )),
                    "reject" => {
                        expected_rejections += 1;
                        if verdict.is_ok() {
                            mismatches.push(format!("{}/{id} accepted", case.id));
                        } else {
                            caught += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        assert!(expected_rejections > 0);
        assert!(
            mismatches.is_empty(),
            "{caught}/{expected_rejections} expected rejections caught; mismatches:\n{}",
            mismatches.join("\n")
        );
    }

    #[test]
    fn settings_patches_reject_unknown_keys_and_apply_known_ones() {
        let mut patch = Map::new();
        patch.insert("correctionMode".into(), json!("intent_aware"));
        patch.insert("correctionRemoveFillers".into(), json!(false));
        let settings = patched_settings(&Settings::default(), &patch).unwrap();
        assert_eq!(settings.correction_mode, "intent_aware");
        assert!(!settings.correction_remove_fillers);
        patch.insert("noSuchSetting".into(), json!(1));
        assert!(patched_settings(&Settings::default(), &patch).is_err());
    }

    #[test]
    fn arguments_require_dataset_and_output() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>()
        };
        assert!(parse_arguments(&args(&["--dataset", "a.json"])).is_err());
        assert!(parse_arguments(&args(&["--dataset"])).is_err());
        let parsed = parse_arguments(&args(&[
            "--dataset",
            "a.json",
            "--out",
            "b.jsonl",
            "--case",
            "x",
        ]))
        .unwrap();
        assert_eq!(parsed.only, vec!["x".to_owned()]);
        assert!(parsed.settings.is_none());
    }

    #[tokio::test]
    async fn provider_failures_insert_the_raw_transcript() {
        let dataset: Value = serde_json::from_str(DATASET).unwrap();
        let mut patch = Map::new();
        // A cloud provider without consent fails before any request is sent.
        patch.insert("correctionProvider".into(), json!("openai"));
        let base = patched_settings(&Settings::default(), &patch).unwrap();
        let case = parse_cases(&dataset, &base).unwrap().remove(0);
        let row = evaluate(&case).await;
        assert_eq!(row["accepted"], json!(false));
        assert_eq!(row["candidate"], Value::Null);
        assert_eq!(row["output"], json!(case.transcript));
    }
}

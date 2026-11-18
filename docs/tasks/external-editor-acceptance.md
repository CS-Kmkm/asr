Goal: Verify real-model Dictate delivery to an operator-enrolled external scratch editor, starting with Notepad.

Scope: Reuse the existing Windows acceptance runner, isolated application/profile, fixed audio and evaluation telemetry. Start with Hold Dictate only; select further editor/scenario trials after the first result. This task does not close the wider acceptance matrix.

Constraints: Preserve existing uncommitted work, normal profiles, weights and all previous outputs. No downloads, environment changes, cloud inference, commits, pushes, Issue changes or security changes. No insertion-guard changes. The operator launches the input-injecting runner from an interactive terminal. Only two dedicated empty scratch controls may be enrolled. The dedicated profile remains Hold ON; do not silently switch to toggle or request unsupported Hold guard scenarios.

Reuse / creation plan: Use scripts/windows_acceptance.py with --target external --recording-mode hold --scenario dictate. Use a unique new output directory for each attempt. Reuse .ai/windows-acceptance-20261117-6f60ee9a/run.json for manifest/profile identity and diagnostic-03-hold/events.jsonl for the current app. The read-only .ai/external-editor-preflight-20261117.py checks readiness without UI or audio operations.

Acceptance criteria:
1. Before operator execution, check a fresh healthy idle heartbeat and profile /2, audio, binary and runner-file identity against diagnostic-03-hold/native-01-dictate/acceptance.json. Recheck in the runner; readiness is time-sensitive.
2. Obtain an actual external acceptance.json for Notepad Hold Dictate. Record enrolled process/version when available, operation ID, outcome, receiver/CER result and identity. Missing/unsupported UIA or focus is BLOCKED, not product failure. No actual result means pending.
3. Review evidence and technical scope independently after substantive changes. Retain browser/VS Code, toggle guards, isolated IME/TSF, clipboard restoration, physical microphone and product benchmark gaps.

Open questions: May decide myself: begin with Notepad because the existing bridge explicitly supports its scratch edit controls, then choose browser/VS Code after observing actual provider compatibility. Operator interaction is required for scratch enrollment and launch; no foreground bypass.

Context: docs/tasks/native-acceptance-continuation.md contains completed native results and their limitations. docs/windows_evaluation.md describes external enrollment and result meanings. scripts/windows_uia_bridge.ps1 defines supported targets. No need to rerun model-quality benchmarks or the completed native matrix.

Status: Deferred at the user's request to prioritize Nemotron preparation. AC1 checked on 2026-11-17: PID 39484 idle with a newly received heartbeat; profile /2, audio, executable file and listed runner files match the prior native Hold result. This does not identify debug frontend contents. AC2 pending operator launch and actual external result. AC3 pending independent review. No external input was injected and no editor acceptance is claimed. Recheck readiness when this task resumes.

Next actions:
1. Operator prepares two empty Notepad scratch windows, launches the explicit Hold Dictate command, and enrolls primary/secondary as prompted.
2. Inspect the new acceptance.json and matching application operation; retain FAIL/BLOCKED attempts and stop on uncertain app state.
3. Choose the next provider or scenario from that evidence. Hold focus/selection/cancel combinations are unsupported by the current runner.

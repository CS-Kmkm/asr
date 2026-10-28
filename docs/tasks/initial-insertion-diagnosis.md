Goal: Identify reproducible causes of failed first text insertion, including Windows IME interaction.
Scope: Diagnosis of current initial insertion and its clipboard/Unicode/IME guards. Preserve all previous modifications; do not apply speculative fixes.
Constraints: Never read/log user text. Use synthetic text in disposable controls. Isolate clipboard probes from the user's clipboard. Keep normal input/IME settings unchanged.
Reuse / creation plan: Reuse Windows backend and native test-control pattern; temporary probes are removed after diagnosis. Persist evidence in an incident note.
Acceptance criteria:
- Observe actual target text or Windows API results, not just mock success.
- Separate clipboard routing/restoration and IME detection from injection-event acceptance.
- State the mechanism and evidence for each reproduced failure; distinguish the user's unresolved app-specific report.
- Remove temporary tracked instrumentation; preserve earlier regression tests and fixes.
Open questions: User confirmed VS Code, reordered insertion and partially uncommitted text. Exact input surface (editor/chat/terminal) requested asynchronously. IME state still to establish in a synthetic reproduction.
Context: src-tauri/src/injection.rs, commands.rs, types.rs; prior NumLock diagnosis is a distinct issue.
Status: Archived historical investigation/design. Superseded by core-dictation-insertion.md and live-dictation.md; current remaining acceptance belongs to issue-58-64-error-and-model-loading.md.
Findings: Current clipboard metadata routes to Unicode; initial UIA pattern/query failures are treated as no composition. Neither proves the reported uncommitted/reordered text. VS Code 1.136.1 uses native EditContext; readonly ime-text-area must NOT be targeted. Early empty-output probes using that textarea or losing editor focus are invalid and excluded.
Artifacts: Temporary Rust/CDP probes copied to C:/Users/Koshi/AppData/Local/Temp/asr-ime-probe-d0fe77a105624efb971b5bc2dbf56410. Product test include and scratch source files removed. Prior product changes preserved. Details in docs/incidents/2026-09-05-initial-insertion-ime-investigation.md.
Next: No standalone action. Follow the active issue-remediation contract.

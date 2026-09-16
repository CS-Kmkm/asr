Goal: Diagnose and fix corrected text failing to replace the provisional Whisper transcript across applications.
Scope: Common Windows provisional insertion path and focused regression coverage. Preserve unrelated changes and user content.
Constraints: Do not weaken focus, input, IME, or exact-text verification guards. Do not retry an unconfirmed queued paste or overwrite its clipboard payload. No provider calls are needed for synthetic reproduction.
Reuse / creation plan: Reuse existing native integration helpers and injection batch tests; add only evidence-driven fixes and durable coverage. Use temporary paths for diagnostic output.
Acceptance criteria:
- Reproduce and distinguish the failure through the real Windows backend (or explicitly document the missing environment boundary).
- A changed synthetic correction replaces the draft exactly once and preserves surrounding text.
- Relevant safety regression tests, library tests, all-target compilation and changed-file formatting pass.
- Record verified mechanism, remaining actual-application limitations, and independent review result.
Open questions: User reports every app retains the draft and the overlay shows replacement underway. The exact guard failing is not yet observed.
Context: Prior correction-replacement-recurrence.md covers late paste confirmation; current batch tests (17) pass. Historical correction requests succeeded, but insertion outcomes are not persisted. Screen phase alone does not prove replacement executed.
Status: Reproduced asynchronous selection defect fixed, verified, reviewed, and running in the rebuilt release app. Unsupported native EDIT remains a stated capability limit.

Evidence (2026-09-14):
- [stated] The built application's real input-monitor helper emitted its ready handshake and exited cleanly on shutdown.
- [stated] A disposable native child EDIT with verified foreground/process identity received `prefix draft suffix`; expected `prefix corrected suffix` failed after actual batch begin/finish. Metadata: IME `Ok(None)`, TextPattern absent (null-interface result), `replacement=None`, begin and finish `PasteUnverified`.
- [derived] When UIA cannot expose composition/text, begin falls back to an untracked draft paste. Finish correctly refuses to overwrite an unconfirmed paste, so the AI output never reaches that target. The native result establishes this failure path, not which UIA capability every user application exposes.
- [stated] The overlay enters finalization before finish runs, so the displayed phase is not evidence that replacement was attempted.
- [decision] The user explicitly requires provisional input to remain. Deferred/corrected-only insertion is rejected. Investigate additional verified Windows text/IME access instead; do not bypass destructive guards.
- [stated] An isolated Chrome textarea exposes both TextPattern and inactive TextEdit composition. Begin confirmed the draft with a replacement range, but finish returned ClipboardOnly because selection readback was still the old caret state immediately after UIA Select.
- [stated] Adding one diagnostic read after Select observed the old state, followed by the expected selection on the next read and successful corrected paste. This isolated an asynchronous selection/readback race, separate from unsupported native EDIT controls.
- [stated] `delayed_selection_is_confirmed_before_corrected_paste` failed before the fix with `prefix draft suffix`, then passed. The fixed batch suite passes 19 tests, including wait-boundary and interruption coverage. The retained ignored Chromium integration test passes without diagnostic output.
- [decision] Treat UIA Select success as request acceptance. Confirm its effect in the batch layer using the existing bounded wait and input/focus/IME guards. Never retry selection or accept a third text state. Reuse confirmation for cancellation and recheck before deletion.
- [unresolved] Native EDIT controls that expose neither text ranges nor composition remain unsupported for safe replacement. The Chromium fix does not claim universal application coverage; a missing provider capability cannot be converted into proof of inactive composition.
- [stated] The running user app is `src-tauri/target/release/local-voice-input.exe`, timestamp 2026-09-07, while current debug/source fixes are newer. Both release and debug monitor helpers successfully completed their ready handshake.

Final verification:
- [stated] Rust library tests: 103 passed, 3 interactive tests ignored. `cargo check --manifest-path src-tauri/Cargo.toml --all-targets`, changed-file rustfmt, and `git diff --check` passed. Existing dead-code warnings remain.
- [stated] The retained `chromium_confirms_provisional_selection_before_replacement` test passed on a real desktop with an isolated Chrome profile, for ASCII and Japanese/CRLF/emoji cases. It verifies provisional insertion, confirmed replacement, and surrounding text through the actual UIA/clipboard/SendInput path; it mocks only input-monitor availability and does not call the AI API.
- [stated] `pnpm.cmd build` passed after retrying with desktop-user permissions; the sandbox rejected esbuild access to a parent directory. No frontend source changes were needed.
- [stated] Independent GPT-5.6 Sol xhigh reviewer found no blocking introduced defect and accepted the selection wait, interruption handling, cancellation checks, regression scope, and explicit native EDIT limitation. This records requested routing, not independently verified runtime model metadata.
- [stated] Temporary diagnostic source and product instrumentation were removed. Pre-existing `.diagnostics/app-dev.*.log` files were preserved.
- [stated] Release build passed after stopping the old executable that prevented Cargo from replacing its file. The rebuilt `src-tauri/target/release/local-voice-input.exe` was started from the repository root. Application data/settings were not modified by this task.

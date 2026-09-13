Goal: Diagnose and fix the recurring failure to replace an inserted draft with AI-corrected text.
Scope: Existing injection pipeline and focused regression coverage; preserve unrelated working-tree changes and user data.
Constraints: Never replace unverified text, bypass focus/input/IME guards, or overwrite a pending paste payload. Do not log user text. State actual-application verification limits explicitly.
Reuse / creation plan: Extend injection/batch.rs and its existing backend-boundary tests where reproduction warrants it; reuse UIA verification and clipboard policy.
Acceptance criteria:
- Reproduce a draft remaining after correction through the actual begin/finish pipeline before changing product behavior.
- Replace a draft exactly once when it becomes verifiable before correction finishes; preserve surrounding text.
- Pending/unverifiable pastes and changed input/focus/text/IME remain protected.
- Relevant regression tests, Rust library tests, all-target compilation, and changed-file formatting pass.
Open questions: Affected application/input surface and displayed error requested from user; actual-app cause remains unconfirmed.
Context: Prior correction-insertion-fix.md fixed clipboard sequencing and UIA identity comparison. Prior native EDIT fixes do not establish current app-specific behavior.
Status: Reproduced pipeline defects fixed; application-specific attribution remains open.

Verification / evidence (2026-09-12):
- [stated] `draft_confirmed_after_initial_timeout_is_replaced_once` failed before the fix: the target contained the draft, but finish returned PasteUnverified. It passes after finish revalidates the complete edit and safety guards. Cases include changed/unchanged correction, repeated completion, and a newer clipboard copy.
- [stated] Existing `clipboard_restoration_waits_for_the_target_edit`, moved to the final-wait boundary, failed before the loop fix and passes afterward. The wait budget remains 50; every wait now has a subsequent read.
- [stated] The recovery safety table covers changed input, unavailable monitor, changed focus/text/selection, active/unknown IME, and unreadable text. Unconfirmed paste coverage still asserts no clipboard overwrite, selection, or retry.
- [stated] Focused injection tests: 15 passed. Rust library suite: 102 passed, 2 interactive tests ignored. `cargo check --manifest-path src-tauri/Cargo.toml --all-targets`, `rustfmt --edition 2021 --check src-tauri/src/injection/batch.rs`, and changed-file `git diff --check` passed. Existing dead-code warnings remain.
- [stated] Independent advisor review found no introduced blocker; it confirmed the recovery guards, unchanged wait budget, pending-payload preservation, and the documented application/clipboard limits.
- [stated] Read-only local history metadata shows the latest two correction requests succeeded and their outputs differ from the transcripts. No user text or provider credentials were displayed.
- [derived] Those two requests rule out absent/unchanged AI output as the sole explanation. They do not identify which insertion guard failed; insertion outcomes are not persisted in the current history schema.
- [decision] Preserve the original pending-paste policy whenever late readback or safety validation fails. Do not infer successful insertion from SendInput or elapsed time.
- [decision] Keep clipboard snapshot ownership unchanged: after an initial timeout the original snapshot has expired, so a recovered replacement restores the clipboard captured at replacement time (the draft or a newer copy). Recovering the original rich clipboard would require a separate ownership change.
- [unresolved] The affected application/input surface and displayed insertion status have not been supplied. No interactive application reproduction was performed, so this is a verified pipeline fix, not proof that every reported application-specific failure is resolved.

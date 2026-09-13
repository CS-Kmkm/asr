Goal: Display and insert the local transcript before API correction completes.
Scope: Dictation command, app state, and provisional insertion; preserve existing working-tree changes.
Constraints: Preserve focus, input, IME, and verified-range guards for replacement. Never retry or overwrite an unconfirmed paste. No changes to model settings or dependencies.
Reuse / creation plan: Extend existing batch insertion and state APIs, with focused backend-boundary regression coverage.
Acceptance criteria:
- Reproduce the missing pre-correction draft when target readback is unavailable.
- Attempt the local draft immediately using the existing additive insertion policy when replacement tracking is unavailable; retain the outcome through correction without duplicate insertion.
- Publish the local transcript in app state before awaiting correction; apply final output on completion.
- Existing replacement, cancellation, clipboard, and input safety tests continue to pass.
- Rust library tests, all-target check, frontend build, and changed-line whitespace checks pass.
Open questions: Actual affected input application is unknown; interactive application verification is not assumed.
Context: commands.rs currently waits for correction when begin_provisional returns None. MainApp displays last_result, which currently changes only in complete(). Existing unrelated uncommitted batch fixes address late paste verification and must be retained.
Status: Complete. Implementation, automated verification, and independent advisor review passed; live application verification remains a stated limitation.

Evidence and decisions (2026-09-12):
- [stated] `unreadable_target_receives_draft_before_correction_finishes` failed twice before the fix: the mock target remained `prefix  suffix` instead of `prefix draft suffix`. It passes after the fix.
- [stated] The focused batch suite passes 17 tests, including prior late-confirmation recovery and safety guards. Draft state publication also passes its focused test.
- [decision] Retain every nonempty draft attempt in a session, with an optional verified replacement range. This keeps insertion outcomes and no-retry rules together rather than duplicating them in the command.
- [decision] Use the existing additive insertion policy when replacement tracking cannot start. Unknown IME state may permit initial insertion under that existing policy, but never destructive replacement.
- [decision] Never replace or cancel an untracked draft. A differing final result can be copied only after a confirmed paste or clipboard-only outcome; an unconfirmed paste retains its original payload. The final text remains visible in the app.
- [derived] This reproduction does not depend on provider latency or API behavior: the omitted draft is observable before correction is invoked. It does not establish the user's application-specific readback failure.
- [derived] Current begin errors arise before target input is queued, although clipboard contents may already have changed. Partial SendInput is represented as queued input; post-queue readback/restore failures return a retained outcome.
- [unresolved] No live microphone/provider/target-application end-to-end reproduction has been performed.
- [stated] Final verification: `cargo test --manifest-path src-tauri/Cargo.toml --lib` passed 99 tests with 2 interactive tests ignored; `cargo check --manifest-path src-tauri/Cargo.toml --all-targets` passed; `pnpm.cmd build` passed. Existing dead-code warnings remain.
- [stated] `rustfmt --edition 2021 --config skip_children=true --check src-tauri/src/injection/batch.rs src-tauri/src/state.rs` and changed-file `git diff --check` passed. Unrelated pre-existing formatting and working-tree changes were preserved.
- [stated] Frontend verification required escalation after esbuild was denied access to the parent directory in the sandbox. PowerShell's pnpm.ps1 execution restriction was avoided using the installed pnpm.cmd entry point.
- [stated] Independent advisor review found no blockers: retained insertion outcomes, optional replacement range, untracked completion/cancellation guards, pending clipboard preservation, and pre-correction state publication match the contract. The initial queued paste still needs live verification in the user's affected Windows control.

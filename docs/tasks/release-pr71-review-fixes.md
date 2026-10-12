Goal: Resolve the confirmed findings of the release PR #71 (dev -> main) review on dev before the release merge.

Scope / non-scope:
- In scope: review findings F1-F9, F11, F12 below, their regression tests, and this contract.
- Out of scope: F10 (decided below), unrelated refactors, Python worker changes, main, and any commit/push without user authorization.

Constraints:
- Baseline dev: fb442b5 (PR #71 head; CI green at that commit).
- issue-12-history.md remains the History contract except where this file supersedes it (F1 preset list and legacy mapping).
- Irreversible insertion follows the selected-text translation pattern (commands.rs, `lifecycle.commit_side_effect` around `replace_selection`): a cancel either wins before anything is pasted or copied, or finds the operation committed and does nothing.
- Rust checks use -j 4. Real Windows insertion timing, devices, and providers remain manual checks.

Reuse / creation plan:
- Extend existing enums, tests (`cargo test --lib`, `scripts/test-*.mjs`), i18n dictionaries, and the existing commit pattern. No new dependencies. New files are limited to node tests under scripts/ and, if needed for testability, one small pure frontend helper for settings-save ordering.

Findings and decisions:
- F1 Legacy `historyRetentionDays: 90` (a main UI option) migrates to `one_month` and the startup purge deletes 30-90-day History. Decision (user, 2026-10-12): add a `three_months` preset (90 days) between `one_month` and `one_year`; legacy days still map to the largest finite preset not exceeding the old limit, so 90 -> `three_months`. Supersedes the preset list in issue-12-history.md.
- F2 Ask: a cancel after the replacement/caret paste was sent reports "Ask cancelled" and drops History although the target changed.
- F3 Speak to edit: a cancel just before the paste makes the fallback copy the edit to the clipboard and records a completed `clipboard_only` edit.
- F4 `reconfigure` interrupts a running load/download before checking whether the worker command changed; API-only field changes on a local backend trigger it.
- F5 Global style profile guidance saves the whole Settings object per keystroke; out-of-order acknowledgements revert the input.
- F6 History Retry emits no `voice-mode`, so the overlay keeps the previous session's mode label.
- F7 Ask search Retry returns the full spoken instruction instead of the derived search query stored by the original run.
- F8 Retry phase messages are missing from i18n.tsx.
- F9 `HistoryAudioPayload.bytes: Vec<u8>` crosses IPC as a JSON number array (issue-12 follow-up F7, payload part only).
- F10 `add_metric` decodes stored settings per insert. Decision (may decide myself): keep. Reading the stored row under the same connection is what serializes the History-off decision with the insert (issue-12 contract); passing caller settings would reintroduce staleness, and the cost is a small JSON parse.
- F11 Duplicate consecutive `is_cancelled` check in `finish_ask`.
- F12 Voice-translation completion sentences duplicate `selection_translation_status`.

Acceptance criteria:
1. F1: `three_months` round-trips through Rust and TypeScript, has a 90-day cutoff, sorts between 1 month and 1 year in the shortening confirmation, and is localized; legacy 90 maps to it; tests cover every legacy value class boundary (6/7, 29/30, 89/90, 364/365). Verify: cargo test, node tests, tsc.
2. F2/F3: Ask (selected replacement and caret draft) and Speak to edit run the irreversible insertion inside `commit_side_effect`; a committed paste completes with History; a cancel before commit neither pastes nor copies. Verify: cargo test with regression tests where seams exist; code review.
3. F4: an unchanged worker command does not interrupt a running request; API fields affect the decision only for the openai-compatible backend. Verify: cargo test.
4. F5: the guidance inputs save on commit (blur), not per keystroke, and an older acknowledgement cannot revert typed text. Verify: node test or tsc plus review.
5. F6/F7/F8/F12: Retry publishes its mode, Ask search Retry returns the derived query, Retry messages are localized and recognized as app messages, completion sentences have one source. Verify: cargo test, node tests, tsc.
6. F9: audio bytes cross IPC as binary; Play and Download keep working with the 50 MB cap. Verify: cargo test, tsc, review.
7. Whole tree: `cargo fmt --check`, `cargo clippy -j 4`, `cargo test --lib -j 4`, `pnpm exec tsc --noEmit`, `node --test scripts/test-*.mjs` pass; one independent audit with valid findings resolved.

Open questions: None.

Context:
- Review evidence: PR #71 review (2026-10-12). Key locators: storage.rs legacy migration (~700), types.rs HistoryRetention (~391), components/HistoryRetentionSelect.tsx, commands.rs finish_ask (~2040-2260), Speak to edit replacement (~1460-1530), selected-text commit (~2599), retry_history_item (~5692), worker_configuration_changed (~5540), asr.rs reconfigure (~1001), ask.rs validate_fixed_search_retry_plan (~480), SettingsPage.tsx PersonalizationProfiles (~430).

Status:
- F1-F9, F11, F12 implemented by three disjoint workers (W1 History: F1, F9; W2 cancellation: F2, F3, F11; W3 Retry/settings/misc: F4-F8, F12) and integrated uncommitted on dev. Whole-tree checks after integration: fmt OK, clippy 20 warnings (all in untouched files), cargo test 589 passed / 0 failed / 5 ignored, tsc OK, node 70/70.
- Decisions: F9 filename/MIME are derived in api.ts (History audio is always WAV), so no metadata command. F4 also makes an API-field-only edit on a local backend no longer count as a model change (it is not refused while recording).
- Independent audit (Codex, requested gpt-6-sol via the VS Code extension's codex.exe, read-only): (A1) an older settings acknowledgement or settings-changed echo can still revert committed profile text through App.saveSettings ordering -> accepted, root cause of F5; (A2) Ask clarification fallback copies outside commit_side_effect -> accepted, same class as F2; (A3) legacy historyRetentionDays 0 maps to 24_hours -> rejected: main's backend validated 1..=3650, so 0 was never a released stored value.
- A2 fixed (`commit_ask_copy`) and confirmed resolved by the same auditor. W2's sweep found no other paste, clipboard write, or browser launch outside a commit in finish_ask or Speak to edit.
- A1 round 1 (latest-only response/rollback/language, echo suppression while saving) integrated; the recheck left R2 (overlapping saves can reach the backend lock in reverse, storing the older whole Settings) and R3 (two failures roll back to an unconfirmed state). Round 2 integrated: App.saveSettings serializes update_settings (one in flight, each sends the settings shown at its turn); a latest-save failure restores only fields changed since the last backend confirmation (field-level on purpose, so unsaved Setup choices survive as before). The same auditor confirmed R2 and R3 resolved with no new queue defect.
- Residual (accepted, not proven by code): R1, an older save's settings-changed echo applied after all saves settle, requires the webview to receive events out of emission order (update_settings emits before it returns, commands.rs ~6018); it relies on Tauri delivering events to a webview in emission order. R4 (an external settings-changed during a save is dropped and then overwritten by the whole-object save) is pre-existing on main. Both need a backend settings revision; candidate follow-up, not in this task. prepareModel and saveCustomModel still call updateSettings outside the queue (pre-existing).
- Outside this task's findings (same cancel-vs-clipboard class, not changed): the voice-translation copy when the target language is missing (commands.rs ~1659) and the dictation/translation final insertion (~1814-1822).
- Final checks on the integrated tree: fmt OK, clippy 20 warnings (all in untouched files), cargo test --lib 591 passed / 0 failed / 5 ignored, storage:: x10 OK, tsc OK, node 77/77. Acceptance criteria 1-7 met except the R1/R4 residual recorded above.
- Remaining manual limits: real Windows insertion/cancel timing, Tauri binary IPC in the webview (Play/Download), overlay labels.
- Done (2026-10-12): committed as eight commits f376ef8..cd403ef and pushed to origin/dev with user authorization; the three temporary agent worktrees and their branches were removed after confirming their changes were in HEAD.
- Handed off: R1, R4, the out-of-queue model saves, and the two dictation clipboard/insertion paths moved to the follow-up contract settings-revision-and-dictation-commit.md. Manual verification limits above remain with the user.

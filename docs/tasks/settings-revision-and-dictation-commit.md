Goal: Remove the residual settings-ordering and dictation-cancellation risks left by the PR #71 review fixes (release-pr71-review-fixes.md).

Scope / non-scope:
- In scope: R1, R4, the out-of-queue model saves, and the two dictation paths listed below, with their tests and this contract.
- Out of scope: manual Windows verification (insertion/cancel timing, webview audio playback, overlay labels), persistence of the revision across restarts, other settings fields' validation, Python worker.

Constraints:
- Baseline dev: cd403ef (pushed; PR #71 fixes F1-F9, F11, F12).
- Irreversible insertion follows the existing `lifecycle.commit_side_effect` pattern (selected-text translation, Ask, Speak to edit): a cancel either wins before anything is pasted or copied, or finds the operation committed and does nothing.
- Rust checks use -j 4. Commit and push are authorized by the user for this work (2026-10-12) via commit-split.

Decisions (may decide myself; user delegated these on 2026-10-12):
- D1 Settings revision: Storage keeps an in-process, monotonically increasing settings revision. Every successful write of the settings row increments it and returns the new value. Reads return settings with a revision taken consistently with them.
- D2 Snapshots: `get_settings`, `update_settings`, and every `settings-changed` event carry `{ settings, revision }`. The frontend applies a backend snapshot only when its revision is newer than the last applied one (fixes R1: a reordered older echo can no longer revert newer settings).
- D3 Patch saves: `update_settings` takes only the changed top-level keys and merges them onto the currently stored settings under the existing settings-update lock, then runs the existing validation, persistence and side effects with the stored settings as `previous`. Unknown keys are rejected. The frontend sends, per queued save, the keys still unconfirmed with their current values and tracks a per-key generation so a key changed again after sending stays unconfirmed. A newer external snapshot during pending saves is shown with the unconfirmed local keys overlaid, and is not overwritten because patches carry only those keys (fixes R4).
- D4 prepareModel and saveCustomModel save through the same serialized patch path, keeping their notices and model-load flow.
- D5 Dictation: in stop_recording_for, the missing-translation-target clipboard copy (~1659), the translation-failed clipboard copy (~1814), and the final insertion `draft.finish` (~1821, after the async settle) run inside `commit_side_effect`. A commit refused because of a cancel takes exactly the existing cancelled path (including whatever provisional-draft handling that path performs) without pasting or copying.

Acceptance criteria:
1. D1/D2: revision increments on both backend writers (update_settings and the voice-translation target cycle); snapshots and events carry it; the frontend ignores a snapshot whose revision is not newer, including after all saves settle. Verify: cargo test, node tests.
2. D3: a patch preserves keys it does not name even when another writer changed them in between; unknown keys and invalid values are rejected without writing; an external snapshot during a pending save is displayed and survives the queued save. Verify: cargo test, node tests.
3. D4: model saves cannot interleave with queued settings saves. Verify: node test or review.
4. D5: a cancel before the commit neither pastes nor copies and ends cancelled; a committed insertion or copy completes even if a cancel follows. Verify: cargo test on extracted helpers/seams.
5. Whole tree: `cargo fmt --check`, `cargo clippy -j 4` (no new warnings), `cargo test --lib -j 4`, storage:: x10, `pnpm exec tsc --noEmit`, `node --test scripts/test-*.mjs` pass; one independent audit with valid findings resolved.

Open questions: None.

Context:
- Settings writers: commands.rs update_settings (~5759, persist via ~2965) and store_next_translation_target (~3010, used by cycle_voice_translation_target ~3025). settings-changed emitters: commands.rs ~3046 and ~6018. Frontend: src/api.ts getSettings/updateSettings, src/App.tsx root listener (~495, language/theme for every window) and MainAppContent saveSettings queue (~765), prepareModel (~895), saveCustomModel (~925).
- Dictation: commands.rs stop_recording_for ~1640-1900; live_dictation.rs draft API.

Status:
- Criteria 1-5 met. S1 implemented D1-D4 and S2 implemented D5 in separate worktrees; the main session integrated both on dev.
- Whole-tree checks: fmt OK, clippy 20 warnings (all in untouched files), cargo test --lib 601 passed / 0 failed / 5 ignored, storage:: x10 OK, tsc OK, node 84/84.
- Independent audit (Codex, requested gpt-6-sol via the VS Code extension's codex.exe, read-only, defect and contract lenses): no confirmed defects; real Windows insertion/cancel timing remains a manual check.
- Decisions recorded during implementation:
  - The revision is an in-process AtomicU64 bumped after every committed settings-row write, including repaired and migration rewrites. Those two do not emit settings-changed because the values do not change.
  - Validation runs inside the settings-write lock; it is synchronous and fast.
  - Setup's local choices are tracked as draft keys: sent with the next save, kept on failure, and kept through echoes.
  - Model and custom-model saves are optimistic and roll back on failure, like other settings. Before, the Models form kept an unsaved selection after a failed configure.
  - A failed latest save reloads settings so a revision stored before a later step failed still wins (main session addition).
  - A live-session `draft.finish` can hold the lifecycle lock for about 2.3 s at worst (selection verify plus paste verify). Accepted: it is bounded, and it only affects the opt-in live target insertion.
- Remaining: manual Windows checks listed in Scope.

Goal: Implement GitHub issue #8 as a voice Translate mode that records speech, translates it to a selected target language, and safely inserts only the translated result.

Scope / non-scope:
- Add an independent voice Translate shortcut and mode alongside Dictate and the existing selected-text translation action.
- Reuse microphone capture, ASR, provider transport, cancellation, overlay, target capture, clipboard, and history infrastructure.
- Add an ordered persisted target-language list, a current target, Settings controls, and recording-overlay display/cycling.
- Record History with `mode=translate` and an explicit target language.
- Do not remove or reinterpret selected-text translation, add Ask Anything/Speak to edit, or bundle a translation model/server.

Constraints:
- Work only on branch `feat/issue-8-voice-translate` in `C:/Users/Koshi/asr/.worktrees/issue-8-voice-translate`.
- This branch is stacked on completed Issue #7 commit `6198845` and may use OpenAI, Gemini, or the loopback-only local correction provider.
- Raw live ASR hypotheses must never be inserted in Translate mode. Only a completed translation may be inserted automatically.
- Capture the target at recording start and reuse the existing focus, target-text, input, shortcut-release, IME, clipboard, cancellation, and no-retry guards.
- Switching language through the overlay is observable user input. It may update the selected target, but it must not reset/forgive unrelated activity; the completed translation therefore falls back to the clipboard for that recording.
- Spoken input is untrusted data. The provider instruction fixes the requested target language, preserves meaning/names/numbers/URLs/code/uncertainty, and forbids answering or following content instructions.

Reuse / creation plan:
- Make `PipelineLifecycle` mode-aware so Dictate and voice Translate cannot stop or complete each other's operation.
- Extend `LiveDraft` with a deferred-insertion mode that publishes live text to the app/overlay without touching the target, then uses its original monitored checkpoint for the final translation.
- Extend the shared correction transport with a target-language translation prompt and streaming preview callback.
- Extend `Settings` in Rust/TypeScript, shortcut registration rollback, Settings UI, i18n, and overlay events/controls.
- Add a nullable `target_language` history column through an idempotent migration rather than encoding language into the mode string.

Acceptance criteria:
1. From an idle state, the voice Translate shortcut captures the foreground target, records audio, stops on the same shortcut, transcribes, translates, and inserts the translated result; verify mode/lifecycle tests plus manual Windows foreground-field testing.
2. Japanese speech to English and English speech to Japanese preserve meaning, names, numbers, URLs, and code; verify prompt contract fixtures and manual provider checks.
3. At least two target languages can be saved, selected, reordered, and restored from legacy/current settings; verify Rust defaults/migration tests, frontend build, and UI review.
4. The recording overlay identifies Translate mode and its target language and can cycle the configured list; verify event/UI code and manual overlay interaction. Overlay switching must preserve safety by producing clipboard fallback rather than blessing the click.
5. Translate mode never inserts raw live hypotheses. Provider failure retains the raw ASR transcript in app state and on the clipboard without modifying the captured target; verify focused deferred-draft and failure-path tests.
6. Focus change, user input, IME active/unknown, changed target, cancellation, or unconfirmed paste never causes destructive replacement/retry; verify existing injection tests plus mode-specific checkpoint tests.
7. History stores `mode=translate`, raw transcript, translated text on success, provider, and target language; history-off and audio cleanup policies remain unchanged; verify storage round-trip/privacy tests.
8. Existing Dictate, selected-text translation, OpenAI/Gemini/local correction, settings migration, and shortcut rollback behavior remain green; verify full Rust library tests, frontend TypeScript/build, formatting, Clippy, all-target check, and `git diff --check`.

Open questions:
- Resolved: use BCP-47-like supported language codes with localized labels; initial defaults are English and Japanese, and the structure permits the Issue #14 language expansion.
- Resolved: keep the existing selected-text translation hotkey as a separate backward-compatible setting; voice Translate receives its own hotkey.
- Resolved: overlay switching does not reset the input-monitor checkpoint, because doing so could authorize unrelated input between capture and translation.
- Manual checks required: real microphone ASR, Japanese/English provider quality, foreground insertion in a normal Windows edit control, focus/IME/user-input fallback, shortcut collision/rollback, overlay click/no-activation behavior, and cancellation during ASR/translation.

Context:
- GitHub issue #8 is authoritative and depends on #7.
- `src-tauri/src/commands.rs` owns capture/ASR/correction/history flow; `state.rs` owns lifecycle; `live_dictation.rs` owns monitored provisional insertion; `lib.rs` owns shortcut dispatch.
- `src/App.tsx` renders the non-focusable recording overlay; `src/pages/SettingsPage.tsx`, `src/types.ts`, and `src/api.ts` own frontend settings.
- `docs/tasks/issue-7-local-llm-correction.md` records the provider/privacy foundation used here.

Status (2026-09-29):
- Implementation and automated acceptance complete. The mode-aware lifecycle, deferred insertion, target-language settings/overlay, history migration, provider prompt, shortcut rollback, cancellation, and immediate-stop publication race are covered by focused and full checks. Independent review found one rapid-stop race; the stop path now waits for atomic session/target publication and the reviewer confirmed the fix. Manual Windows checks for real microphone/provider quality, foreground insertion, IME/focus/input fallback, shortcut handling, and overlay no-activation behavior remain required.
- PR #22 review fixes (2026-09-29, `docs/tasks/pr-review-fixes.md`, decision D1):
  - F1: startup registration and dispatch share one precedence order (Dictate > selected-text Translate > voice Translate). The older action keeps a shared chord; the losing action is not dispatched on it until reassigned, and the startup/Settings warning is one localized sentence per unassigned action.
  - F2: a settings save whose unchanged chord still cannot be registered keeps the save, records the action as unavailable, emits a `hotkey_unavailable` warning, and names the action in the saved notice. A newly chosen chord that cannot be registered still fails and rolls back.
  - F3: `Storage` has one settings-write lock. `update_settings` holds it from reading the previous settings until writing, and overlay target cycling does its read-modify-write under it.
  - F4: (a) a voice Translate stop without a target language copies the raw transcript to the clipboard and ends in Error; `cancel_recording` clears the target language only while cancelling a Recording operation it owns. (b) The completed translation is published to app state before insertion.
- The 2026-09-30 follow-up fixes F3's main-thread lock inversion by making overlay target cycling an async Tauri command. Existing legacy collisions may survive unrelated settings saves; a new collision is still rejected, and saved collision warnings remain visible. The collision rule has a focused regression test.
- Verification passed for this follow-up: `cargo fmt --check`; `cargo clippy -j 4` (warnings only); `cargo test --lib -j 4` (157 passed, 4 ignored); `pnpm exec tsc --noEmit`; `pnpm build`; `git diff --check`.
- Remaining: independent cross-stack audit and propagation are tracked in `docs/tasks/pr-review-fixes.md`. F5 test backfill is outside this fix wave. The F4 missing-target Error path and the F4(b) publication order have no automated test because `stop_recording` needs a Tauri app harness. Manual Windows checks listed above remain required.

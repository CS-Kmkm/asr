Goal: Let one key do Aqua Voice-style dictate-or-edit (issue #84, parent #80): with an opt-in setting, the Dictation shortcut edits the selection read at the press and dictates when nothing is selected, never replacing text it could not see, with clear feedback and per-take measurements for p50/p95 comparison.

Scope / non-scope:
- In scope: opt-in `smartDictateEdit` setting (default off) on the existing Dictation shortcut; one read-only probe of the original target and selection at the press; rule-based routing (no model inference); copy-only policy for an unknown selection; overlay route label; Smart-edit Undo hint and copy-only feedback (Japanese i18n); routing table and guard tests over the mock selection/focus backend; `take_spans` instrumentation (route, key presses, stop-to-final) and `scripts/summarize_takes.py`.
- Non-scope: a separate Smart shortcut, screen OCR or full-screen context capture, sending selected text anywhere new (Edit keeps its existing provider path), removing or relaxing any insertion guard, a new Undo mechanism (the target app's own Ctrl+Z is the undo), ASR runtime/catalog files (`asr_worker/backends.py`, `src/asr-model-catalog.json`, `src-tauri/src/model_catalog.rs`, ModelsPage).

Constraints: Existing per-mode shortcuts, hold/toggle and automatic voice mode behavior unchanged; automatic voice mode takes precedence over Smart. The lifecycle keeps the Dictate mode of the pressed key so the same key stops/releases/cancels a Smart take. Nothing is pasted unless the routed mode's existing guards pass (focus/target identity, IME, input activity since the press, selection content for Edit, protected targets). Records stay content-free (fixed codes and numbers). No new dependencies, no network.

Design:
- Routing table (`src-tauri/src/smart_dictate.rs`, probe in `injection::batch::probe_selection`):
  | Probe at the press | Route | Delivery |
  |---|---|---|
  | Readable, nonempty selection in an external, unprotected field (IME not composing) | `edit` | Speak to edit over exactly that snapshot selection; monitor started with the Dictation chords |
  | Readable field whose selection endpoints are collapsed (a selected embedded object without characters is not empty), or unreadable text with a collapsed selection | `dictate` | Final-only Dictate into the probed field: the draft keeps the press-time input checkpoint and focused-field identity, never inserts live text, and every paste requires an explicitly empty selection and inactive IME |
  | Anything else: unreadable selection, password/elevated target, this app, no foreground field, focus changed while reading, open IME conversion | `copy_only` | Dictate with this app as the target: result goes to the clipboard and History only, never pasted; notice detail `selection_unknown` |
- Unknown-selection policy: Dictate-without-replace (copy only) rather than cancel, so speech is never lost; the overlay says "Smart: copy only (selection unknown)" at the start and the completion says to paste with Ctrl+V.
- A selection that cannot be monitored, or an Edit route whose provider/privacy preflight fails, refuses the start with the existing Speak to edit / privacy message instead of falling back to Dictate (which could replace the selection).
- Smart Edit: the input monitor starts before the selection is read and any input observed while it is read refuses the start; the replacement also requires the UI Automation identity of the field the selection was read from until the paste is sent (another field with identical text is refused).
- Hold-to-talk: the snapshot and the start-request time are taken at key-down, in parallel with the 200 ms hold delay; the microphone still opens only after it. Toggle presses read the field right after the lifecycle claim.
- Only a Dictation shortcut press routes; the app's Record button and dictation tests keep plain Dictate.
- Feedback: overlay names the route for 2.5 s; a confirmed Smart edit shows "Selection edited. Press Ctrl+Z in the app to undo."; failures keep the existing clipboard/History recovery (History Retry for failed recognition).
- Instrumentation (schema v5, additive): `stop_to_final_ms`, `route`, `key_presses` on `take_spans`; mode recorded is the routed mode. Key presses = shortcut presses attributed to the take until its stop claims it (start press, toggle stop, cancelling press, Esc); hold release is not a press; a clipboard-only take's manual Ctrl+V is not counted. Diagnostics cohorts group by route; `scripts/summarize_takes.py` prints p50/p95 of `record_ready` (hotkey to record-ready), `stop_to_final`, `stop_to_delivery` (pasted takes only = stop to confirmed paste) and `key_presses`, plus success/clipboard-only/cancel/failure rates per route and mode.

Acceptance criteria:
1. Routing decision table and opt-in rules hold, with every unknown state copy-only. Verify: `cargo test --lib smart` (smart_dictate, commands::smart_start_tests, live_dictation copy-only test).
2. A routed Edit replaces only the unchanged original selection; selection change, focus move, IME composition, typed text, observed keystrokes and an unreadable target never paste, and the result stays recoverable. Verify: `cargo test --lib smart_dictate`.
3. Settings toggle is opt-in, disabled while recording or with automatic voice mode; overlay route label, Smart-edit Undo notice and copy-only detail render in Japanese. Verify: `pnpm test` (settings-controls, window-views).
4. Per-take route, key presses and stop-to-final are stored and summarized. Verify: `cargo test --lib take_trace`, `cargo test --lib storage::`, `uv run --offline --with pytest pytest asr_worker/tests/test_summarize_takes.py`.
5. No regression. Verify: `uv run --offline --with pytest pytest`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check` (in `src-tauri`), `pnpm test`, `pnpm build`, `node --test scripts/test-*.mjs`.
6. Native checks below are done by the user and recorded here; issue #84 is not closed before that.

Native-check list (Windows 11, Smart Dictate/Edit on, both toggle and hold-to-talk):
- VS Code editor and VS Code integrated terminal; a browser `<input>`/`<textarea>` and a contenteditable (e.g. a chat box); Notepad; Windows Terminal (PowerShell).
- In each: (a) no selection -> text inserted, overlay "Smart: dictating"; (b) select text -> overlay "Smart: editing the selection", the selection is replaced, Ctrl+Z restores it; (c) apps whose selection cannot be read (expected: terminals, possibly VS Code/Electron) -> "copy only", nothing typed, Ctrl+V pastes the result.
- Guards: with a selection, during recording click elsewhere / change the selection / type a key / leave a Japanese IME conversion open -> no replacement, result on the clipboard with the matching notice; a password field -> copy only; Esc and a second press cancel without insertion; a clipboard held by another app.
- Plain Dictation/Edit/Translate/Ask shortcuts with Smart off and on behave as before.
- Measurement: run 20+ takes per route, then `uv run scripts/summarize_takes.py` (add `--dev` for a debug build, `--since` to restrict) and record record_ready / stop_to_final / stop_to_delivery p50/p95, key presses per take and success/clipboard-only/failure rates; compare with the same tasks using the separate Dictation and Edit keys (issue AC 5).

Open questions: None blocking. May decide myself: Smart lives on the Dictation key behind a setting (like automatic voice mode) instead of a new shortcut; copy-only instead of cancel for an unknown selection.

Context: issue #84; `docs/tasks/automatic-voice-routing.md`, `docs/tasks/measurement-and-structural-latency.md` (take_spans M1); `src-tauri/src/{smart_dictate,sessions,commands}.rs`, `injection/batch.rs`, `pipeline/{stop,edit,dictate_translate,take_trace}.rs`, `storage/{metrics,schema}.rs`; `src/windows/RecordingOverlay.tsx`, `src/pages/SettingsPage.tsx`.

Status (2026-11-12, branch `feat/issue-84-smart-dictate-edit`):
1-5. Implemented with automated checks passing: cargo test 904 passed / 7 ignored, clippy -D warnings, fmt, vitest 200, node --test 41, pytest 338 passed / 10 skipped (pytest from the local uv cache with `--offline --with pytest`; the project environment has no pytest), pnpm build.
Independent audit: Codex (read-only) found 2 P1 and 3 P2 issues, all fixed in `fix(dictate): keep Smart press-time field and selection guarantees` with fake-layer regression tests: (P1) Smart Dictate re-captured the checkpoint/field identity after the model wait and could accept another field or replace a selection made meanwhile -> press-time `PressSnapshot`, final-only insertion with explicit empty-selection guard; (P1) Smart Edit accepted another field with the same text -> `PinnedField` identity pin until the paste; (P2) Edit monitoring started after the selection read -> monitor first, input during the read refuses; (P2) empty selected text treated as empty selection -> collapsed-endpoint check, otherwise copy-only; (P2) hold mode probed 200 ms late and understated record_ready -> key-down snapshot and timestamp. A scoped recheck of the fixes has not been run yet.
Trade-off: Smart Dictate always requires a known-inactive IME at paste (the existing after-input guard), so apps whose IME state cannot be read get copy-only; verify natively.
Migration numbering: this branch adds `take_spans` columns as schema migration v5; issue #85 (PR #102, unmerged) also adds v5/v6 in `storage/schema.rs`. Whichever merges second must renumber its steps after the other's (append-only; never reorder released steps).
6. Not done: Windows native verification and real-use comparison (agent sessions cannot take the foreground).
Remaining: recheck of the audit fixes; native checks; PR.

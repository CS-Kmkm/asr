# Stop-to-insert latency fixes

Goal: Remove the stop-to-insert delays identified in the 2026-11-01 audit (see `docs/asr-performance-audit.md` and the Claude review in session `6043cb09…`) without weakening insertion safety, and show live recognition as subtitles in the recording overlay.

Scope:
- F1 IME composition false positive: `ime_active` treats any non-null `GetActiveComposition` range as an active composition. Historical takes into development apps waited the full 5 s settle limit and ended as `clipboard_only:ime_active` (3 of 13 takes). Treat a composition range without text as inactive; keep a real composition blocking.
- F2 Waiting feedback: while the deferred final paste waits for an unready target (focus moved, IME composing), tell the user in the overlay what to do instead of waiting silently for 5 s.
- F3 Settle quiet window: count the 350 ms quiet window from the last observed input, not from the start of settling.
- F4 Input-monitor shutdown: `InputMonitor::shutdown` polls up to 500 ms on the completion path after the paste. Move the wait/kill off the critical path while keeping availability/generation state correct for the next take.
- F5 Pre-AI shortcut-release wait: skip or defer `wait_for_shortcut_release` before AI when nothing consumes it (deferred final insertion), keeping the final paste's modifier/shortcut safety.
- F6 Live subtitles: live recognition stays on; its partial text is shown as a subtitle line in the recording overlay while recording, and the last live text stays visible while the final recognition runs until the draft/correction preview replaces it. README already promises overlay partials.
- F7 Early live stop: signal the live task to stop before audio finalization so no new live decode starts during stop; join afterwards.

Non-scope: AI correction policy (model, skipping, speculative or deadline fallback; deferred by the user), new measurement work, record-start optimizations, paste verification timeouts, worker/ASR decode settings, history/storage changes.

Constraints: Preserve insertion safety (focus, selection, IME, user-activity, clipboard restore), cancellation and ownership semantics. No transcript, audio or window title in logs/status. Live text may be sent only to app windows. Existing tests must pass; add focused tests for changed behavior. Commits follow `commit-split` on branch `perf/stop-to-insert-latency-fixes` (decided locally: the main checkout's `perf/stop-to-insert-latency` holds another session's uncommitted work, so integration uses a separate worktree branch from c92e8d6).

Reuse / creation plan: Extend existing modules (`injection/accessibility.rs`, `live_dictation.rs`, `input_monitor.rs`, `pipeline/{stop,dictate_translate}.rs`, `recording_overlay.rs`, `src/windows/RecordingOverlay.tsx`, `src/i18n.tsx`) and their tests. No new crates or npm packages.

Acceptance criteria:
1. F1–F7 implemented as scoped, each with a focused automated test where the code path is testable off-device. Verify: `cargo test --lib`, `pnpm test`.
2. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `tsc` (`pnpm build` type check) and `pytest asr_worker/tests` pass on the integrated tree.
3. One independent audit (defect, contract lenses) of the branch diff; valid findings fixed.
4. Native Windows verification (IME in the affected development app, overlay subtitles, real paste) is listed as pending unless performed.

Open questions: none blocking. Exact overlay wording/layout may be decided locally.

Context: `docs/asr-performance-audit.md`; historical metrics show settle-limit waits of ~5.1 s after correction for `ime_active`/`target_changed` takes and 0.4–0.8 s otherwise.

Status: implementation and automated checks complete; native verification pending.
- Criterion 1: done. F1 (empty composition range is inactive; unreadable range still blocks), F2 (1 s unready hint via the overlay, shown in the processing view), F3 (quiet window from the monitor's last-input stamp), F4 (helper exit/kill on a detached thread; generation/availability updated synchronously), F5 (pre-AI shortcut wait only for live target edits; the deferred final paste's settle/`unchanged_since`/`safe` guards cover held keys), F6 (`live-subtitle` event to the overlay only; room reserved before the recording is shown), F7 (`LiveTask::signal_stop` before audio finalization). Each has focused tests except the Windows-only F1 `GetText` path and F4 helper code.
- Criterion 2: done on the integrated tree: `cargo test --lib` 766 passed/6 ignored, clippy `--all-targets -D warnings`, `cargo fmt --check`, `pnpm test` 165 passed, `tsc --noEmit`, node script tests, `pytest asr_worker/tests` 166 passed.
- Criterion 3: done. Codex audit route rejected the configured model (`gpt-6-sol` unsupported for the ChatGPT account); per bindings an independent Opus agent audited (defect, contract). Findings fixed: recording hints no longer survive a phase change, wait-hint styling/wrapping, subtitle room reserved before showing the recording, README `live_drain` boundary note; scoped recheck done. Accepted: a busy hint during processing is now visible in the processing view.
- Criterion 4: pending. Not run on Windows: whether the affected development app returns an empty composition range (F1 premise), overlay subtitle/hint rendering, real paste/IME behavior.
- Known pre-existing items left as-is: `ime_unknown` can still arise when a UIA dispatch fails or times out (single busy worker), possibly misattributing a slow text read.

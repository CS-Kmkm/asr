# Measurement and structural latency follow-ups

Goal: Implement the remaining automatable items from `docs/asr-performance-audit.md` after #75: M1 (per-take span instrumentation across all terminal paths), M2 (benchmark parity with production), the S2 remainder (History and dictionary-candidate saves off the next-take path), A5 (opt-in incremental final ASR) and C3 (opt-in speculative correction), and prepare a native verification checklist for #74/#75 and this work.

Scope (one worker per slice; disjoint write sets per wave; integration branch `perf/measurement-structural`):
- Wave 1
  - M1 instrumentation: a per-take span record correlated by an ephemeral take id, covering record-ready, model ensure, audio stop, live drain, ASR, shortcut-release wait, route, AI stage, settle, paste, helper exit, persistence, and the time until the next take can be accepted; a terminal outcome record for every exit path (confirmed paste, unverified, clipboard-only, insertion failure, early failure by stage, cancellation by stage) so spans can be grouped by outcome and missing spans counted. New records are versioned/namespaced so they are not mixed with the historical metrics.
  - M2 benchmark parity: `scripts/bench_asr.py` reproduces production request parameters (locale, dictionary prompt in the production format, device/compute type reporting), idle gaps between requests, and optional concurrent live-preview load; documented in `docs/benchmarks.md`.
  - A5 incremental final ASR (opt-in, default off): during recording, confirm segment boundaries and recognize confirmed segments with the final-recognition settings; at stop, recognize only the unconfirmed tail and join by boundaries (never by removing duplicated strings). Fall back to full-recording recognition whenever identity conditions do not hold (settings, prompt, backend/model changes, worker reset, errors, missing segments).
- Wave 2 (after wave 1 is integrated)
  - S2 remainder: move the Dictate/Translate History save and dictionary-candidate save onto the ordered completion queue, with a frontend refresh event so the History/Dictionary pages stay correct; History save stays before temp-audio cleanup; Delete all / retention changes and retry/re-run paths wait for or see pending saves.
  - C3 speculative correction (opt-in, default off): after stop, correct the last live hypothesis in parallel with final ASR; adopt it only if the final transcript and every correction input (settings, dictionary hints, personalization guidance, provider/model) are identical; otherwise cancel it and run the normal correction. Record a content-free hit/miss metric (and the live-vs-final match rate even when C3 is off).
- Main session: native verification checklist (`docs/tasks/native-verification-checklist.md`) covering #74, #75 and this task.

Non-scope: C2 (short-text skip, correction deadline fallback, model/provider change: needs user decision), default changes to recognition/correction, Edit/Ask completion reordering, cue/target-capture order, running real-device or provider measurements.

Constraints: Preserve insertion safety, cancellation and ownership semantics, consent gating and privacy: no transcript, audio, window title or prompt text in logs or metrics; take ids are random, ephemeral and never linked to History ids in exported diagnostics; metrics follow the existing retention/Delete all policy. New settings default off and keep current behavior byte-for-byte when off. A5 issues no new provider requests (cloud ASR keeps full-recording recognition unless live preview already sends the same audio to that provider under existing consent; otherwise A5 is local-backend only). C3 settings copy states that a miss costs an extra AI request and that a cancelled take may already have sent the hypothesis. Existing tests pass; each change has focused tests where testable off-device. Workers commit on their own worktree branch with Conventional Commits; the main session integrates.

Reuse / creation plan: Extend existing modules (`pipeline/*`, `storage/metrics.rs`, `live_dictation.rs`, `asr_worker/*`, `scripts/bench_asr.py`, settings types/UI) and tests. No new crates/npm/Python packages. New files only for a clearly separate unit (e.g. a span recorder module, an incremental-ASR segment module).

Acceptance criteria:
1. Each scoped item implemented or explicitly escalated with evidence. Verify: per-slice focused tests and worker reports.
2. Integrated tree passes `cargo test --lib`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `pnpm test`, `pnpm exec tsc --noEmit`, node script tests, `uv run pytest asr_worker/tests`.
3. One independent audit (defect, contract) of the integrated diff; valid findings fixed.
4. Native verification checklist written; native/real-device verification and speed/accuracy measurement listed as pending unless performed.

Open questions:
- Span storage shape (new table vs metrics columns), outcome codes: may decide myself (worker records decision).
- A5 segment confirmation rule, worker scheduling against preview decodes, prompt conditioning across segments: may decide myself within constraints (worker records decision and an offline replay comparison).
- C3 enablement and C2 policy defaults: needs user confirmation (C3 ships default off).

Context: `docs/asr-performance-audit.md` (IDs, locators, constraints), `docs/tasks/remaining-latency-improvements.md` and `docs/tasks/stop-to-insert-latency-fixes.md` (prior slices), `docs/benchmarks.md`, `docs/worker_protocol.md`.

Status: wave 1 starting.

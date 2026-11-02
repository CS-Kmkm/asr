# Remaining latency improvements

Goal: Implement the remaining non-policy speed-ups from `docs/asr-performance-audit.md` (A1, A3, A4, N1, S1, S2, U1, U2, selected-text translation off the async owner, record-start overlap, Ask streaming preview) without changing recognition or correction defaults or weakening safety/privacy.

Scope (one worker per slice; disjoint write sets):
- W1 ASR worker and live recognition: A1 direct PCM16 WAV load for faster-whisper with fallback; A3 opt-in cheaper live-preview decode (new setting, default off; final recognition unchanged); A4 cheap new-audio/speech gate before the full live audio preparation (decoded audio identical when a decode runs); N1 reuse of cloud ASR HTTP connections (no extra requests).
- W2 storage and completion: S1 WAL truncate only when retention deleted something or a checkpoint retry is pending; S2 move post-delivery history/metrics/dictionary-candidate/temp-audio work for Dictate/Translate off the next-take critical path with explicit ordering and ownership.
- W3 startup, worker lifecycle and recording start: U1 bounded GPU diagnosis overlapping ASR preload where independent; U2 skip `uv sync` when a recorded environment fingerprint matches, with repair fallback; selected-text translation UIA/monitor/replace work on a blocking owner like Edit/Ask; overlap independent record-start preparation.
- W4 Ask: stream the generation as a preview in the answer panel with operation-scoped partial/cancel/final handling.

Non-scope: AI correction policy (C2), speculative correction (C3), incremental final ASR (A5), full end-path instrumentation (M1), changing recognition/correction defaults, measurement campaigns, Edit/Ask completion reordering (W2), cue-before-microphone order and original-target capture order changes.

Constraints: Preserve insertion safety, cancellation and ownership semantics, consent gating and privacy (no transcript/audio/window title in logs; live/Ask text only to app windows; no new provider requests). Privacy deletions (Delete all, retention changes) must not be overtaken by deferred writes. New settings default to current behavior. Existing tests pass; each change has focused tests where testable off-device. Workers commit on their own worktree branch with Conventional Commits; integration into `perf/remaining-latency` is done by the main session.

Reuse / creation plan: Extend existing modules and tests; no new crates/npm/Python packages. New files only for a clearly separate unit (e.g. a completion queue module).

Acceptance criteria:
1. Each scoped item implemented or explicitly escalated with evidence. Verify: per-slice tests.
2. Integrated tree passes `cargo test --lib`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, `pnpm test`, `tsc --noEmit`, node script tests, `pytest asr_worker/tests`.
3. One independent audit (defect, contract) of the integrated diff; valid findings fixed.
4. Native/real-device verification and speed measurement listed as pending unless performed.

Open questions: Exact preview decode parameters, gating thresholds and queue design may be decided by the worker within these constraints (record the decision in the report). Anything requiring a default/policy change needs user confirmation.

Context: `docs/asr-performance-audit.md` (IDs and locators), `docs/tasks/stop-to-insert-latency-fixes.md` (prior slice, already merged in #74).

Status: started 2026-11-02 on branch `perf/remaining-latency` from `main` f19a842.

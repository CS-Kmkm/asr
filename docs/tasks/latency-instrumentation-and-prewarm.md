# Latency instrumentation and provider prewarm continuation

Goal: Finish and independently audit the unfinished instrumentation/provider-prewarm work from Claude Code session `8813f06a-f7f4-4271-ace8-bd525dbe2a34`, and make the planned decoder comparison reproducible without changing recognition defaults.

Scope: Review and fix the existing eight-file uncommitted change; accurately document timing and prewarm behavior; add meaningful offline tests for connection reuse/privacy and timing observation; inspect available evaluation audio; prepare the existing benchmark's production locale input. Perform appropriate Rust/Python checks and build verification where feasible.

Non-scope: Broad architectural optimizations from the separate `6043cb09...` report, new models, recognition-default changes, live-target/clipboard safety changes, paid provider inference, microphone capture, app restart/termination, commits or push. Do not overwrite existing experiment audio/results or the recovered investigation artifacts.

Constraints: Existing source changes are the starting implementation, not disposable work. HEAD prewarm is best effort, must use the same pooled client, respect consent/mode selection, and send no credentials/text/audio. First output is a subset of AI time, not a network-connect metric. Historical diagnostic cohorts omit early failures/cancellation; do not silently redefine existing events or claim comprehensive timing coverage. Preserve retention, cancellation, ownership and insertion safety. No actual speedup claim without a matched experiment.

Reuse / creation plan: Extend existing transport tests and benchmark harness/tests. Use a new ignored `.ai/test-results/` prefix for command logs and local evidence. Human-facing README wording stays English to match the existing document; conversation is Japanese. Keep the separate completed performance investigation unchanged except for a factual follow-up pointer if needed.

Acceptance criteria:
1. Existing latency/prewarm change has a defect/maintainability/contract audit, with confirmed findings fixed and scoped rechecks complete.
2. Offline transport tests exercise the actual prewarm request, absence of sensitive headers/body, shared connection reuse, and provider/consent gating as appropriate. Timing tests cover the first nonempty output semantics without timing-fragile assertions.
3. README descriptions match actual timing boundaries and best-effort connection warming; decoder defaults remain unchanged.
4. Available audio is inventoried without reading unrelated personal data; the existing benchmark can explicitly reproduce production locale. Real decoder accuracy comparison is performed only with suitable reference audio; otherwise the missing evidence is stated precisely.
5. Relevant checks pass and logs are retained; source-only verification is not presented as real provider/microphone performance evidence.

Open questions: Broad additional optimization implementation is a separate decision. This bounded continuation may choose small fixes/tests needed for the existing work and benchmark preparation. Recorded evaluation audio is not assumed available; synthetic fixtures may verify tooling but cannot establish microphone accuracy or overall speed.

Context: `docs/asr-performance-audit.md`, current git diff, `src-tauri/src/correction/transport.rs` and tests, `src-tauri/src/pipeline/{stop,dictate_translate}.rs`, `scripts/bench_asr.py`, `scripts/test_bench_asr.py` if present, `docs/eval_set/` and preserved `.diagnostics/` fixtures.

Status: completed (criterion 4 partially; see below).
- Criterion 1: done. Independent audit by Claude Code (session `6043cb09…`, not the implementer) found two metric-cohort defects, both fixed: `insertion_settle` is no longer recorded with a zero wait when a failed translation is only copied, and `*_first_output` is recorded only for completed AI requests. README wording updated accordingly.
- Criterion 2: done. Transport tests cover the anonymous prewarm, consent/mode gating and connection reuse; first-output tests avoid wall-clock assertions.
- Criterion 3: done. README timing and best-effort warming text match the code; decoder defaults unchanged.
- Criterion 4: partial. `scripts/bench_asr.py --language` reproduces the production locale (with tests). No reference audio inventory or decoder accuracy comparison was performed; the user deferred measurement work on 2026-11-01.
- Criterion 5: done. `cargo test --lib` (755 passed, 6 ignored), `cargo clippy --all-targets -D warnings` and `pytest asr_worker/tests` (166 passed) pass on the final tree. No real provider, microphone or foreground insertion run.

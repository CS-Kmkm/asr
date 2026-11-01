# ASR implementation performance audit

Goal: Complete the repository-wide performance investigation started in Claude Code session `6043cb09-3484-41ae-a331-5050f85cedf3`, with a source-backed Japanese report and a prioritized measurement/implementation roadmap.

Scope: Recover prior findings, independently check the current implementation, trace recording start through insertion and readiness for the next take, and assess local/cloud ASR, live processing, AI modes, storage, UI and startup. Investigate streaming and speculative processing as design options.

Non-scope: Implement optimizations, change settings/defaults, run paid provider requests, record microphone audio, alter experiment outputs, commit or push. The eight existing uncommitted files belong to the separate comparison session `8813f06a...` and must be preserved.

Constraints: Distinguish measured results from hypotheses. Historical latency samples are not a current baseline or a matched 3-second KPI benchmark. Preserve input-target, cancellation, clipboard and privacy safeguards. No speedup estimate is a measured result without a reproducible experiment.

Reuse / creation plan: Recover the old workflow's results into a new `.ai/performance-audit-20261031/` directory, preserving provenance. Reuse existing benchmark scripts and evaluation references. Write the final human-facing report in Japanese at `docs/asr-performance-audit.md`; agent-facing contracts and worker evidence use English. No shared-memory checkpoint is requested.

Acceptance criteria:
1. Recovered work has an inventory of completed, failed and unverified coverage, grounded in the original workflow/logs.
2. Current code has a file-and-line critical-path trace, explicit timing boundaries and a coverage map for all major subsystems.
3. High-priority recommendations are independently checked against code, deduplicated, and include applicability, risks and a concrete validation plan; speculative claims remain labeled.
4. The Japanese report separates the existing uncommitted instrumentation/prewarm changes from proposed work and identifies remaining measurement gaps.
5. One independent evidence/technical review is completed, or explicitly remains pending if unavailable. Review findings are resolved before claiming completion.

Open questions: Implementation and benchmark settings require a follow-up decision; this task is investigation only. Investigation organization and prioritization may be decided locally.

Context: Original Claude workflow `wf_9559301c-63d.json` and its script/subagent logs; current pipeline, worker and insertion code; `docs/benchmarks.md`; `docs/notes/live-dictation-segmentation.md`; current git diff. Old global ASR task entries are historical, not this task's active contract.

Status:
- Criterion 1: done. Recovered all 12 successful finder outputs (147 candidates), all 54 completed verifications (5 confirmed, 48 partial, 1 refuted), workflow progress and logs. The other 93 verifications failed. Recovery provenance and candidate inventory are in `.ai/performance-audit-20261031/`.
- Criterion 2: done. Three disjoint read-only workers plus main-session source checks covered the critical path, worker/audio/live processing, shell/storage/frontend and AI modes. The timeline and timing boundaries are in `docs/asr-performance-audit.md`.
- Criteria 3–4: done. The report distinguishes historical data, current source observations, preexisting changes and unmeasured proposals. Individual historical microclaims are retained as candidates, not all claimed independently verified.
- Criterion 5: done via native fallback review. Claude CLI 2.1.292 was detected, but the external Opus audit invocation was rejected by automatic approval review for missing explicit source-payload/destination authorization. No alternate external route was used. Independent native reviewer (requested `gpt-6-sol`, effort `xhigh`) raised three findings: metric cohort/early-exit bias, live-loop cadence wording, and direct connection-reuse observability. All were adopted and scoped rechecks passed; no remaining findings in the report review. Review scope and limitations are recorded in `.ai/performance-audit-20261031/review-resolution.md`.
- Local validation: inventory counts and selected evidence paths verified; `git diff --check` passes. No changes to the original eight modified implementation/README files; per-file diff counts remain unchanged. No app/build/benchmark/provider inference was run.
- Status: complete for investigation and report. Final artifact: `docs/asr-performance-audit.md`, linked from the task index. No shared-memory entry corresponds to this task, so historical unrelated ASR entries were left untouched.
- Follow-up, not unfinished investigation work: agree implementation/measurement scope using the report's ordered plan; validate real performance and foreground input safety before claiming improvement.

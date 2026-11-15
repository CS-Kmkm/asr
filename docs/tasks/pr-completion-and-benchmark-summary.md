Goal: Merge the completed implementation PRs #102-#105 safely and provide a small benchmark score report for model context.

Scope: integrate existing PRs, resolve conflicts and schema numbering, independently audit new integration changes and #104's last fix; add an offline aggregate-only benchmark summarizer and a generated Japanese comparison overview. Close issues only if their actual acceptance criteria are complete.

Constraints: preserve the main checkout's four unrelated uncommitted files, trained weights and all experiment outputs. No model/audio downloads, benchmarks, setting changes, history rewrites or branch/worktree deletion within this integration/summary slice. Commits, pushes and merges necessary for this request are authorized.

Reuse / creation plan: use the existing PR worktrees, storage migration machinery, scripts/score_asr.py output and committed issue-89 benchmark JSON. Add scripts/summarize_bench.py with focused synthetic tests and a short generated overview next to the detailed benchmark files. Keep complete JSON inputs unchanged; the overview is a derived view, never an input to score/baseline calculations.

Acceptance criteria:
1. #103 merges before retargeting and merging #105; #102/#104 merge with their schema steps preserved and numbered without collisions. Verify remote PR state and integrated storage tests, including upgrading an existing v6 DB.
2. Independently review #104's last fix and all new substantive changes. Run integrated Rust, frontend, Python and script checks; distinguish pre-existing or environment failures from regressions.
3. Generate compact aggregate reports from score_asr bench/manifest output and the committed Japanese multi-model comparison. Retain source identity, units, model/runtime/device and evaluation conditions, group scores, missing/failure warnings. Omit per-item results and transcription text; never recompute scores or substitute zero for missing values. Verify synthetic tests and exact source metric correspondence.
4. Commit and publish the summary separately from PR integration fixes. Verify it materially reduces input size; report byte reduction without claiming tokenizer-exact savings.
5. Check issue completion honestly. #84/#85/#87/#89 have outstanding native/evaluation conditions and stay open unless evidence resolves them. Verify GitHub issue states.

Open questions: none blocking. Interpret the requested token reduction as a derived compact report, keeping detailed evidence intact. The user subsequently authorized a second 1.7B transformers download for #89 and chose to gate AI-correction dictionary suggestions under personalization for #85. Those follow-ups belong to their issue work, not this summary change. The user also requested completing issues sequentially, including remaining acceptance criteria, before closing them.

Status:
1. Done: #103 merged, #105 retargeted to main and merged, #102 task-index conflict resolved and merged, #104 merged with Smart migration v7. v4/v6 upgrade regression preserves existing rows and NULLs.
2. Done: integrated Rust 942 passed / 7 ignored, frontend 213, build pass, node 41, Python 393 passed / 10 skipped, Clippy -D warnings and fmt pass. One native independent reviewer audited the final Smart fix, migration and summary. It found same-window focus movement during probing, per-item issue lists in summaries, and unknown comparison fields potentially carrying item text. All were fixed and the bounded recheck found no remaining supported findings. Claude external transmission was rejected by automatic approval review; the user declined transmission. Nothing was sent there. Audit limit: native Windows behavior and unreleased Smart-only v5 databases were not established by the review.
3. Done: seven summary tests and existing scorer self-test pass. Snapshot rows match all nine source models and 18 domain rows. Missing values remain null; warnings and missing/divergent/unmatched counts remain explicit. Complete original results are unchanged.
4. Ready to publish: issue-89 comparison view is 7,889 bytes versus 36,273 input bytes (78.25% smaller by bytes; tokenizer-specific savings are not measured). Generated output exactly matches regeneration after review fixes. Summary is a presentation-only view; provenance SHA256 refers to the unchanged detailed input.
5. Done for this slice: all four issues have unmet native/evaluation acceptance criteria and remain open. Further issue completion proceeds one at a time, starting with #87. No issue was falsely closed, and no branches/worktrees or experiment outputs were removed.

Usage: `uv run scripts/summarize_bench.py SCORE.json --output NEW-context.json`. Use `docs/eval_set/results/issue-89-qwen3-context-v1.json` for model context; read the detailed JSON only when investigating distributions or provenance. Next: publish the reviewed summary separately, then continue #87 native checks under its existing task documents.

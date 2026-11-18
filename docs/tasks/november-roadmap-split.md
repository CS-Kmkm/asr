This record describes the initial branch split. The later measurements and owner-approved closure are recorded in [November roadmap automated evaluation](november-roadmap-automated-evaluation.md).

Goal: Organize the completed local roadmap implementation into reviewable branches and reconcile GitHub issue status with actual evidence and the owner's acceptance decision.

Scope: Native acceptance tooling, application streaming/policy and its coupled features, benchmark resource sampling, and related evidence. Keep pre-existing benchmark/device and documentation edits outside the split when separable. The owner accepted practical-operation testing as complete. Preserve original run statuses and distinguish comparative performance/quality work, which was not waived. No publication or merge assumed from local commit authorization.

Acceptance criteria:
1. The original main ref and unrelated working changes are preserved. Each local commit has a coherent intent and Conventional Commit message.
2. Verify staged paths, cleanliness, and meaningful regression checks. Record branch dependencies if stacked.
3. Reconcile relevant GitHub issues using actual scope and evidence. Close only applicable completed issues. Leave unmeasured comparison and deferred research open.

Context: docs/tasks/issues-82-85-integration.md, remaining-feature-implementation.md, native-acceptance-continuation.md and nemotron-app-integration.md. The current release build was accepted by the user.

Status: Local branch split complete. The branches are stacked in this order: `feat/benchmark-resource-sampling` (7a4fc40), `test/windows-acceptance-runner` (a8aab38), `feat/stateful-streaming-policy` (c137ca2), `feat/confirmed-style-learning` (9bac8e2), `feat/local-llama-server` (0f5f277), then the closeout documentation. Main and the remote remain at the baseline. Older edits to scripts/bench_asr.py device metadata and docs/windows_verification.md remain in the worktree.

The owner accepted practical-operation checks as complete and confirmed the rebuilt Nemotron-enabled app worked. Existing PASS/BLOCKED files retain their original verdicts. CPU/CUDA single-clip and Rust streaming-worker tests passed. No cross-product latency, resource, or blind editorial comparison is implied. GitHub #81–#85 and roadmap #80 have remaining comparisons or acceptance criteria. #86 and #89 were already closed. Research #108 stays deferred.

Independent read-only review identified one misplaced evaluation call, now part of the Windows acceptance commit, and two misleading command comments, corrected in the closeout. The style and local-server UI cases now enter in their respective commits. The branches are local. A future issue PR needs the PR scope gate and permission to publish.

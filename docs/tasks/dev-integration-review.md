Goal: Independently verify Claude's completed issue #7-#15 integration on dev and correct confirmed defects.

Scope / non-scope:
- Review the integrated implementation against issue-local contracts and the completed integration record on main. Focus on the #11/#13/#15 merge resolutions and the subsequent correctness fixes.
- Fix reproducible defects and run appropriate regression checks; commit and push coherent corrections under the existing user authorization.
- Do not alter main's implementation, rewrite history, reopen completed issues without a concrete requirement gap, or remove worktrees.

Constraints:
- Preserve the shared decisions in pr-review-fixes.md and dev-integration.md. Real Windows device/provider/UI behavior remains a manual verification limit.
- Rust checks use -j 4. Independent audit is read-only; the reviewer must not implement or delegate further.

Reuse / creation plan:
- Reuse the issue contracts, existing Rust/Python/frontend tests, and remote CI on the exact reviewed head.
- This file records the bounded follow-up review; dev-integration.md is the completed earlier contract.

Acceptance criteria:
1. Confirm remote PR, issue and exact-head CI status and issue-tip ancestry.
2. Inspect changed execution paths and contracts; record and resolve confirmed correctness defects.
3. Obtain one independent defect/contract audit and recheck substantive fixes.
4. Run relevant checks on the final tree and publish any authorized corrections; report remaining manual limits.

Open questions: None.

Context:
- Baseline dev: 966aafd. Critical merges: da8280b (#11), f099f54 (#13), 3903147 (#15).
- Correctness changes: be5b0ef, 1ffe5ce, 2356512, 9796d96, aff2119, 966aafd.
- The completed integration report is docs/tasks/dev-integration.md on main.

Status:
- Criterion 1 complete: GitHub confirms PRs #20-#28 merged into dev, issues #7-#19 closed, baseline exact-head CI successful, and all nine feature tips are ancestors of dev.
- Criterion 2 complete: Fixed one Medium integrated frontend defect in 0c27e7b: candidates/history were cached across page navigation and shortcut-driven completion, and deleted candidate spans remained visible after retention changed. Visible retained data now refreshes on page/lifecycle/retention changes; Never candidates are hidden immediately; stale requests are invalidated. A second refresh after the settings transaction acknowledges persistence supersedes optimistic pre-purge reads. Existing backend policy and delete-all candidate semantics are preserved.
- Criterion 3 complete: One native independent defect/contract reviewer found the same Medium defect and no other confirmed defects in the scoped implementation. A bounded recheck identified the optimistic-save ordering gap, resolved in the final delta; the final recheck found no confirmed residual defect. Preferred Claude Opus audit was unavailable because the installed CLI version is incompatible. The reviewer used combined merge diffs and adjacent callers after sandbox permissions blocked remerge diffs; main-session remerge inspection succeeded.
- Criterion 4 complete: 11 frontend event/IPC tests passed (6 retained-data tests plus 5 shortcut tests); TypeScript check and production build passed; 17 Rust lifecycle tests and the expired-candidate listing test passed; git diff --check passed. New frontend regressions fail against baseline 966aafd, including direct reproduction of a cached candidate remaining visible under Never. [Exact-head CI for fix 0c27e7b](https://github.com/CS-Kmkm/asr/actions/runs/37086703877) succeeded in all three jobs: frontend typecheck/tests, Rust fmt/clippy/library tests, and Python worker tests. Unchanged backend/Python full-suite verification is supplied by that CI rather than repeated local runs.
- Publication: Fix 0c27e7b pushed to origin/dev without force. This verification record accompanies a separate documentation commit. The review is complete; no additional issue/PR state changes are needed.
- Limits: Event/IPC tests do not emulate a browser. Real Windows devices, shortcut registration, selection/insertion timing, and real local/external providers remain manual checks. Previously documented Low follow-ups remain outside this bounded correction.

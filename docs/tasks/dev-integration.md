Goal: Confirm that issues #7-#15 are adequately implemented, integrate all nine issue branches into a new `dev` branch, push, record PRs #20-#28 as merged into `dev`, and close issues #7-#15 and roadmaps #16-#19.

Scope / non-scope:
- Scope: requirement review of each issue branch head against its live issue body and issue-local contract; creating `dev` from `main`; ordered `--no-ff` merges into `dev` with cross-stack conflict resolution; checks on the integrated tree; independent audit of the conflict resolutions; pushing `main` (docs-only commits), the issue branches and `dev`; retargeting PRs #20-#28 to `dev`; closing issues with evidence comments; checking and closing roadmaps #16-#19.
- Non-scope: merging anything into `main`, force-push or history rewrite, deleting branches/worktrees, `test/issue-7-15-integration` (stale local branch, left untouched), real Windows/provider manual verification (recorded as limits).

Constraints:
- User decisions (2026-10-02): create `dev` from `main` after pushing main's local docs commits; retarget PRs to `dev` so GitHub records them merged; check and close roadmaps #16-#19 after their children close.
- Merge order into `dev`: #7 -> #8 -> #9 -> #10 -> #12 -> #14, then #11 -> #13 -> #15. Each merge is an ordinary `--no-ff` merge commit. Resolutions keep both sides' behavior and the shared decisions D1-D7 in `docs/tasks/pr-review-fixes.md` (in particular #14 `shortcuts.rs` `Routes` stays the final shortcut design, and #15 conservative mode stays byte-for-byte equal to #11's personalized prompt).
- A requirement-review finding of Blocker/High is fixed in its originating issue branch (then propagated) before integration; Medium and lower findings are recorded and disclosed unless they are small local correctness fixes.
- Rust checks use `-j 4`. Do not run a Rust test binary in the background while rebuilding the same target (Windows executable lock).
- Commits: Conventional Commits in English, one intent per commit, Claude attribution trailer.

Reuse / creation plan:
- Reuse the issue-local contracts `docs/tasks/issue-<n>-*.md`, the 2026-10-02 review `docs/tasks/current-implementation-review.md`, and `docs/tasks/search-cancel-shortcut-swap.md`.
- New: this contract; a `dev` worktree at `.worktrees/dev`.

Acceptance criteria:
1. Each issue #7-#15 has a requirement-by-requirement review against its live issue body and contract with no unresolved Blocker/High (read-only review per issue group; findings recorded below).
2. `dev` contains every issue branch tip; merge resolutions keep both sides' fixes (`git show --remerge-diff <merge>` review).
3. The integrated `dev` tree passes `cargo fmt --check`, `cargo clippy -j 4`, `cargo test --lib -j 4` (src-tauri), `pnpm exec tsc --noEmit`, `pnpm build`, the frontend component tests run by CI, Python worker tests, and `git diff --check`.
4. One independent read-only audit of the conflict resolutions finds no unresolved Blocker/High.
5. Branches are pushed without force; CI on `dev` is green; PRs #20-#28 show merged into `dev`; issues #7-#15 are closed with an evidence comment; roadmaps #16-#19 are checked and closed.

Open questions:
- None open. Decisions above were confirmed by the user on 2026-10-02.

Context:
- Stacks and per-branch history: `docs/tasks/pr-review-fixes.md`, `docs/tasks/current-implementation-review.md`, `docs/tasks/search-cancel-shortcut-swap.md`.
- Unpushed branch heads at start: #10 0f6f2eb, #12 c113f24, #14 337d9f5 (all other issue branches equal their remote heads).
- Trial merges show conflicts between #14 and #13/#15 in commands.rs, correction.rs, lib.rs, storage.rs, types.rs, App.tsx, i18n.tsx, HistoryPage.tsx, SettingsPage.tsx, types.ts; #13 vs #15 conflicts only in i18n.tsx.

Status (2026-10-03):
- Criterion 1 done. Three read-only requirement reviews (#7-#9, #10/#12/#14, #11/#13/#15) found no Blocker/High. Three Medium defects were fixed in their originating branches with regression tests: #10 be5b0ef (search command words matched as whole words), #12 1ffe5ce (Retry loads the current model first), #15 9796d96 (ordered-list markers excluded from the intent-aware fact check). #12 2356512 adds the Edit/Ask History labels. Propagation: #10 -> #12 merge 6d8c6b5, #12 -> #14 merge 4c1de22. Remaining Low findings are listed in each issue's closing comment.
- Criterion 2 done. `dev` = main 0d3d63a plus `--no-ff` merges ed161fc (#7), cd17214 (#8), 3f9f606 (#9), 3976f7f (#10), 51626ba (#12), 1dc0256 (#14), da8280b (#11), 58fa622 (review fixes via #14), f099f54 (#13), 3903147 (#15), then aff2119 and 966aafd. Every issue tip is an ancestor of `dev`.
- Integration decisions (may decide myself): Dictate/Translate derive the app context from the captured target; Edit/Ask use global dictionary entries and no style profile; Retry routes by the saved app category; dictionary candidate text follows `HistoryRetention` (none while Never, purged past the window including before listing, kept under Forever) and legacy `historyEnabled`; candidates come only from AI-corrected Dictate, never from translations; "delete all history" keeps candidates (#13 semantics; follow-up).
- Criterion 3 done on `dev` 966aafd: cargo fmt --check, clippy (warnings only: the #14 set plus #11's unused import), cargo test --lib 306 passed / 4 ignored, pnpm exec tsc --noEmit, node --test scripts/test-shortcut-settings.mjs (5), pnpm build, pytest asr_worker/tests 56 passed (needs `uv run --extra serve`, as CI), git diff --check.
- Criterion 4 done. The Codex `gpt-6-sol` route was unavailable (HTTP 400 for a ChatGPT account), so one independent Opus agent audited the fixes and the #11/#13/#15 resolutions: no Blocker/High; M1 (candidate listing ignored the retention window) fixed in aff2119 and rechecked by the same auditor; L4 comment fixed in 966aafd; L1/L2/L3/L5 recorded as follow-ups.
- Criterion 5 done. main, dev and #10/#12/#14/#15 pushed without force; PRs #20-#28 retargeted to `dev` before the push and recorded merged by GitHub. CI succeeded for `dev` 966aafd (run 37080886532) and every pushed issue head. Issues #7-#15 closed as completed with evidence comments (issuecomment-5963540022 .. 5963543794); roadmaps #17, #18, #19 and #16 checked and closed.
- Limits: real Windows device, provider and UI behavior unverified; local `test/issue-7-15-integration`, issue branches and worktrees left untouched.

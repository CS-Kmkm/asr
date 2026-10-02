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

Status:
- In progress: criterion 1 (requirement review), criterion 2 (integration).

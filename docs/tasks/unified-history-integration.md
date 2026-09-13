Goal: Produce one coherent branch containing the reviewed Issue #1-#3 baseline, all valid local correction/injection work, and consistent semantic UI colors, with an intelligible commit history.

Scope / non-scope:
- Preserve and integrate existing local behavior changes for clipboard safety, provisional replacement, early draft display/insertion, spoken correction, and overlay feedback.
- Integrate the semantic notification color fix on top of the Issue #1-#3 baseline.
- Preserve intentional test-suite cleanup as a separate test-only intent after verifying it does not remove unique behavioral coverage.
- Consolidate work into a new branch based on `origin/main`; do not rewrite published commits or mutate the dirty primary branch after its current work is safely committed.
- Exclude `.diagnostics/` runtime logs and do not delete existing worktrees, branches, artifacts, or user data.

Constraints:
- The canonical baseline is `origin/main` at `cb2e974`, which already contains the reviewed Issue #1-#3 merges.
- Existing working-tree changes belong to the user and must not be discarded.
- Preserve focus, input, IME, target-text, clipboard, and no-retry safety guards.
- Keep translation, localization, and persisted settings behavior from `origin/main` while resolving conflicts.
- Use Conventional Commits in English and one revertible intent per commit where the existing mixed `injection/batch.rs` change permits it.

Reuse / creation plan:
- Cherry-pick only the unique pre-Issue local commits (`0075078` through `430d6ea`, excluding duplicate Issue #1/#2 implementations) onto a fresh consolidation branch.
- Commit the current provisional-insertion/draft work and test cleanup by intent before transplanting them.
- Cherry-pick the semantic color implementation `16b9100`; retain work-management documents in separate docs commits.
- Add no dependencies and no compatibility layer.

Acceptance criteria:
- `git log` on the consolidation branch shows the reviewed `origin/main` baseline followed only by unique, purpose-labeled commits; verify with graph/log and patch review.
- Current uncommitted source/test changes are represented in commits or explicitly excluded as runtime artifacts; verify against the original dirty-tree file inventory.
- Issue #1 translation, Issue #3 localization, provisional draft insertion/replacement, and semantic info/success/warning/error colors coexist without dropped fields or UI paths; verify by focused diff review.
- Notification text/background contrast remains at least 4.5:1 for all four severities; verify by deterministic color calculation.
- `pnpm.cmd exec tsc --noEmit`, `pnpm.cmd run build`, `cargo fmt --check`, `cargo clippy`, `cargo test --lib`, `cargo check --all-targets`, and the full Python worker suite pass on the consolidated HEAD.
- No `.diagnostics/` file is tracked and the primary worktree retains no unstaged source/test/document change after its changes are safely transferred; verify with `git status` in both worktrees.

Open questions:
- Remote publication and default-branch integration require explicit approval if the execution environment requests it; local consolidation may proceed.
- Test-only deletions resolved 2026-09-13: recreate only the redundant factory/default/constructor assertions on the consolidated baseline. Retain unload cleanup, malformed transcript rejection, translation prompt separation, and legacy correction-default coverage because independent review found unique regression value.

Context:
- `docs/tasks/correction-replacement-recurrence.md` and `docs/tasks/local-draft-before-correction.md` describe the current uncommitted injection/state work and prior verification.
- `docs/tasks/open-issues-parallel.md` records the Issue #1-#3 implementation and integration.
- `fix/issue-2-color-semantics` contains the unmerged semantic color fix (`16b9100`) and its task record (`245896a`).
- The primary worktree is on `fix/correction-insertion` and contains unrelated runtime logs under `.diagnostics/` that remain untracked.

Decisions (2026-09-13):
- Keep the late-confirmation and early-draft changes in one implementation commit. They share `injection/batch.rs` state semantics, and separating them would create an uncertain intermediate contract.
- Transplant `0075078`, `a40b96b`, `a7e820f`, `891e1e0`, `4a2a009`, and `430d6ea`; exclude duplicate Issue commits/merges `286ed20`, `2f6c361`, `d6a37fb`, and `e3b064a`.
- Recreate the updated Issue/task documentation as a final work-management commit rather than transplanting stale `c42370f` unchanged.

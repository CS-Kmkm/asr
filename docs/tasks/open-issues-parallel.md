Goal: Implement every currently open GitHub issue in an independent branch and worktree, with one subagent responsible for each implementation.

Scope / non-scope:
- Issue #1: Add a Windows global action that reads selected text, translates Japanese to English or English to Japanese through the configured correction provider, and safely replaces the selection. Add the minimum settings needed for its shortcut and instruction. Do not add new providers or send audio.
- Issue #2: Replace the black/green visual theme with an accessible neutral/light surface and blue accent palette. Preserve layout, behavior, and recording-overlay semantics.
- Issue #3: Add persisted Japanese/English UI language selection and localize all primary navigation, pages, notices, and recording overlay copy. Do not translate user data, model identifiers, diagnostics payloads, or provider responses.
- Do not modify or integrate the existing uncommitted correction changes in the primary worktree.
- Each issue remains in its own worktree and branch for review; the orchestrator does not merge the issue branches together.

Constraints:
- Base all issue branches on the latest origin/main, not the dirty fix/correction-insertion branch.
- Preserve privacy: selected text, clipboard data, API keys, and transcripts must not appear in logs or status events.
- Reuse the existing correction provider clients, settings persistence, shortcut registration, injection safety, and frontend patterns.
- Each subagent may write only inside its assigned worktree and must commit its implementation with a Conventional Commit message.

Reuse / creation plan:
- #1 extends the existing correction, settings, global-shortcut, clipboard/injection, and Settings UI paths. New focused modules/tests are allowed when required to preserve separation of concerns.
- #2 primarily changes src/styles.css; component changes are allowed only for accessibility semantics required by the palette.
- #3 may add a small dependency-free i18n module/context and updates existing frontend pages plus the persisted Settings contract in Rust/TypeScript.

Acceptance criteria:
- #1: With non-empty text selected in a normal Windows edit control, the configured shortcut translates Japanese input to English and English input to Japanese, replaces only the captured selection when safe, and leaves the result on the clipboard with a clear error on unsafe replacement or provider failure. Empty selection is rejected without an API request. Focused Rust tests and `cargo test` pass; `pnpm run build` passes.
- #2: No black/green theme remains in the main application; text, focus, hover, disabled, success, warning, and error states have distinguishable contrast in the new palette; the narrowest responsive layout remains usable. `pnpm run build` passes and the diff contains no behavior changes.
- #3: A user can switch between Japanese and English in Settings, the choice survives restart, and all primary UI navigation/pages/notices/overlay labels use the selected language. Dynamic/user/provider content is preserved verbatim. `pnpm run build` and focused Rust settings tests plus `cargo test` pass.
- For every issue: orchestrator confirms the diff stays inside issue scope, verifies the reported commit/worktree, and reruns the narrowest relevant checks.

Open questions:
- Whether issue branches should later be merged together: resolved 2026-09-10; user explicitly requested conflict-resolved merge of PRs #4, #5, and #6 into `main`.
- Exact translation model behavior for mixed-language selections: may decide myself; use dominant Japanese-script detection, translating Japanese-dominant text to English and otherwise to Japanese, because it is deterministic and needs no extra UI choice.
- Translation customization depth: may decide myself; expose one optional instruction field and retain a safe fixed translation contract.

Context:
- GitHub issues #1, #2, and #3 retrieved on 2026-09-09.
- Read README.md for privacy/workflow constraints; src/App.tsx, src/pages/*, src/styles.css for UI; src-tauri/src/{lib,commands,correction,injection,storage,types}.rs for native behavior.
- Existing uncommitted files src-tauri/src/correction.rs and src-tauri/src/correction_prompt.rs in the primary worktree are unrelated user work and must not be copied, reverted, or integrated.

Completion status (2026-09-09):
- #1 done in `C:\Users\Koshi\asr\.worktrees\issue-1`, branch `issue/1-selection-translation`, commit `c0c43f0` (`Refs #1`); independent defect review findings fixed; Rust 99 tests and frontend production build passed.
- #2 done in `C:\Users\Koshi\asr\.worktrees\issue-2`, branch `issue/2-ui-refresh`, commit `1b165c0` (`Refs #2`); frontend production build and diff check passed.
- #3 done in `C:\Users\Koshi\asr\.worktrees\issue-3`, branch `issue/3-i18n`, commit `2108544` (`Refs #3`); independent defect review findings fixed; Rust 96 tests and warning-free frontend production build passed.
- Branches intentionally remain separate and unmerged. Worktrees intentionally remain present for user review.

Review follow-up (2026-09-10):
- PR #6 / Issue #1: change translation direction detection so ordinary Japanese containing product names, URLs, or code is not misclassified; update recording/translation shortcuts as one transaction so swaps succeed and failures restore the old pair; prevent concurrent whole-settings saves while editing the translation instruction; apply Rust formatting; rerun Rust and frontend checks.
- PR #5 / Issue #2: review recommendation is Approve. Make no non-blocking CSS restructuring in this pass; confirm the pushed head and CI/check status only, and rerun the frontend build if remote status is unavailable or stale.
- PR #4 / Issue #3: inventory user-visible hard-coded strings across primary pages/components, localize the remaining Models page descriptions and option labels plus any equivalent omissions found, preserve dynamic/provider content verbatim, and rerun frontend plus relevant Rust checks.
- For changed branches, add a new Conventional Commit referencing the corresponding issue; do not amend reviewed commits. Push each updated branch and confirm the remote head matches local HEAD.

Review follow-up completion (2026-09-10):
- PR #6 / Issue #1 fixed through follow-up commits `c1f87ff`, `56b47d1`, and `9fb182d`. Translation direction now uses a fixed model auto-detection contract instead of a brittle character heuristic; shortcut swaps unregister/register both keys as one unit and rollback removes all new keys before restoring all old keys; translation-instruction editing saves on blur/Enter rather than every keystroke; Rust formatting is clean. Independent defect re-review found no remaining finding. `cargo fmt --check`, `cargo clippy`, 98 Rust library tests, `pnpm exec tsc --noEmit`, and `pnpm run build` passed locally.
- PR #5 / Issue #2 required no blocking code change. Local branch and remote head both remained `1b165c0`, the diff remained `src/styles.css`-only, GitHub frontend/Rust/python-worker checks were successful, and the local production build passed.
- PR #4 / Issue #3 fixed through follow-up commits `c045af4` and `b7971ea`. Remaining Models copy and model-type labels plus equivalent primary-UI omissions were routed through the typed dictionary while dynamic content remains verbatim. Independent localization review found no remaining finding. `pnpm exec tsc --noEmit` and `pnpm run build` passed locally; the prior reviewed head's Rust and python-worker GitHub checks were successful and this follow-up does not change those areas.
- Final remote heads: Issue #1 `9fb182d42464bc5adbb9d777a67b42fe06dbf72d`; Issue #2 `1b165c040ffb30fb6dd0816f209d4fe9b5afc8d7`; Issue #3 `b7971eaa1b6870ea8075a9a1e88853156707dcaa`. Push-triggered and pull-request-triggered GitHub Actions runs for the changed Issue #1 and #3 heads completed successfully for frontend, Rust, and python-worker.

Integration scope (2026-09-10):
- Merge reviewed heads for PR #5, then PR #4, then PR #6 in a dedicated integration worktree based on current `origin/main`; retain merge commits so each PR/issue remains traceable.
- Resolve conflicts by preserving all three accepted behaviors. In particular, any Issue #1 translation settings introduced into an Issue #3-localized component must use the typed i18n dictionary and persist the same Rust/TypeScript settings fields.
- Run the repository CI-equivalent checks on the integrated tree: `pnpm exec tsc --noEmit`, `cargo fmt --check`, `cargo clippy`, `cargo test --lib`, and `uv run --with pytest pytest asr_worker/tests -q`; also run `pnpm run build`.
- Push the verified integrated HEAD to `origin/main`, then confirm remote `main`, PR merged state, and post-push CI. Preserve all existing issue worktrees and the dirty primary worktree.

Integration completion (2026-09-10):
- Created `C:\Users\Koshi\asr\.worktrees\integration-open-issues` on branch `integration/open-issues-1-3` from `origin/main` `3c5234d`.
- Merged PR #5 as `567f9f9`, PR #4 as `d4e8705`, and PR #6 as `42b199a`. The sole textual conflict was `src/pages/SettingsPage.tsx`; it was resolved by retaining the translation hotkey/instruction behavior and localizing its four static labels through the typed dictionary.
- Independent integration review found that the newly introduced Rust translation status notices were not localized. Added the exact eight messages to the English/Japanese dictionaries and `appMessageKeys` in follow-up commit `cb2e974` (`Refs #1`, `Refs #3`); independent re-review found no remaining finding.
- Integrated-tree checks passed: frontend TypeScript and production build; Rust format, clippy, and 98 library tests; Python worker 57 tests (one dependency deprecation warning).
- Fast-forwarded `origin/main` from `3c5234d` to `cb2e974cec7a8ea7b3cf86892b7f3d49e1f60015`. GitHub reports PR #4, #5, and #6 as merged. Post-merge CI run `34456021593` completed successfully for frontend, Rust, and python-worker.

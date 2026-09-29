Goal: Address the actionable 2026-09-27 GitHub review findings for issue branches #7-#15 without losing their per-issue ownership or safety boundaries.

Scope / non-scope:
- Scope: confirmed defects and test gaps described in the latest review comments on issues #7-#15; preserve the existing dependency stacks and update each affected issue branch.
- Scope: verify whether inferred findings reproduce, fix those that do, and explicitly record hardware/provider-only checks that cannot be run here.
- Non-scope: close issues, open PRs, merge into main, push branches, install external model servers, or add OS-wide audio control.

Constraints:
- Branch stacks are #7 -> #8 -> #9 -> #10 -> #12 -> #14, and #11 -> #13 / #15. Keep issue-specific commits and changes attributable to their respective issue.
- Do not rewrite published history. Propagate upstream fixes with ordinary merge/cherry-pick only after reviewing diff and conflict risk.
- Preserve privacy, guarded external side effects, legacy settings, and existing user data. No transcript or API-key logging.
- Local Conventional Commits were authorized by the user; push, PR, and main merge remain outside this task's authorization.

Reuse / creation plan:
- Extend existing provider, input-safety, History, Dictionary, Settings, and prompt modules and issue-local tests.
- Reuse issue-local contracts and the GitHub review comments as the requirements source; do not create a second implementation of an existing mode.
- Add an integration worktree only if needed to prove the #7 and #11/#15 provider contract together; do not silently treat a leaf branch as integrated.

Acceptance criteria:
- #7: URL editing works from empty/partial drafts, local output budget is suitable for thinking models or its limit is clearly controllable, and transport tests cover authorization/proxy/redirect behavior (focused tests).
- #8/#9: launch handles colliding legacy shortcuts safely; Translate/Edit post-publication persistence failure cannot turn successful side effects into command failure; mode-specific guard/deferred-draft tests exist (focused Rust/UI checks).
- #9/#10: reject empty edit instructions, support bounded large rewrites/answers, and keep injected paste verification separate from physical shortcut state (focused tests plus manual timing note).
- #10: planner prompt/schema/site tokens match parser, search query derives deterministically from spoken input, irreversible effects have a commit point, answer panel can dismiss without stealing focus, and Ask UI/i18n is present (focused tests/build).
- #11/#13: profile precedence and display are truthful, dictionary confirmation never corrupts manual entries, candidate detection/dedup and CSV/alias/hint handling preserve data (focused Rust/UI checks).
- #12: retained-audio playback CSP, text-only fallback on audio persistence failure, Retry deletion race/mode label, retention/audio cleanup tests are addressed (focused Rust/UI checks).
- #14: shortcut registration cannot deadlock or abort startup for a legacy collision/occupied chord, alternate stop chord is ignored as input, rollback compatibility and localized errors are addressed (focused Rust/UI checks).
- #15: valid Japanese numeric/self-correction and profile edits are accepted while invented facts are rejected, with real input/output fixtures; local-provider integration is tested in an explicit integration branch (focused tests).
- Every substantive slice has passing relevant checks and an independent read-only audit. Unverified Windows/provider behavior is stated, not claimed.

Open questions:
- May decide myself: implementation order, issue-local test seams, and which inferred review concerns warrant a code change after reproducing them.
- Needs user confirmation: any change that would publish, merge, close issues, or install/control external services.

Context:
- GitHub issue #7-#15 comments posted 2026-09-27 (GPT re-audit and Claude Opus 5.5 audit).
- Existing implementation plan: docs/tasks/issues-7-15-implementation.md.
- Existing per-issue contracts: docs/tasks/issue-<number>-*.md in the corresponding worktree.

Status:
- Done: issue-local review fixes for #7-#15 have independent audits, passing focused/full relevant tests and local Conventional Commits. #11 fixes are merged into #13 and #15; #7 fixes are merged into #8. Those integrated trees passed Rust and UI builds.
- Done: a separate local `test/issue-7-15-integration` worktree combines local correction with intent-aware validation and personalization. Its stub local-server test covers accepted and rejected numeric self-correction; 157 Rust tests passed (4 ignored), UI build passed, and independent merge audit found no concrete defect.
- Done: #8 -> #9 -> #10 -> #12 -> #14 were propagated with ordinary local merge commits. The #12 merge passed 187 Rust tests (4 ignored), UI build, and independent audit; the #14 merge passed 203 Rust tests (4 ignored), UI build, and independent audit.
- Follow-on PR request (2026-09-29): issue branches #7-#15 were pushed and nine issue-scoped Draft PRs were opened (#20-#28). Bases follow the two dependency stacks; #13 and #15 had unrelated diff noise removed before publication. Main and the test-only integration branch were not pushed; no PR was merged.
- Manual-only: Windows hotkey/IME/overlay timing, real microphone, provider quality, and retained-audio playback remain unverified.
- Next: perform the documented manual Windows/provider checks and review Draft PRs before marking them ready; merge only after separate review and authorization.

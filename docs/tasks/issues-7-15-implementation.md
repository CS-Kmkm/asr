Goal: Implement every current actionable GitHub issue (#7-#15) in its own branch and worktree, preserving a reviewable dependency-aware history.

Scope / non-scope:
- Implement #7 local LLM correction, #8 voice Translate, #9 Speak to edit, #10 Ask Anything, #11 personalization/application context, #12 History parity, #13 Dictionary parity, #14 Settings parity, and #15 intent-aware Dictate.
- Treat #16-#19 as tracking/roadmap issues only. Do not create duplicate implementations or code worktrees for them.
- Keep each issue's feature changes and focused tests in that issue branch. Do not merge issue branches into `main` unless the user separately requests integration/publication.
- Do not delete existing worktrees, branches, diagnostics, trained artifacts, or user data.

Constraints:
- Base independent roots #7 and #11 on `origin/main` at `28113f0f3a13eb0ce66305bb0f285bfc48e21778`.
- Stack the voice/history/settings line as #7 -> #8 -> #9 -> #10 -> #12 -> #14 so later schemas and settings cover real modes rather than placeholders.
- Stack #13 and #15 separately on completed #11 so dictionary `app_scope` and intent-aware correction reuse the established application-context routing.
- Existing uncommitted work in `.worktrees/issue-7-local-llm-correction` belongs to this task and must be preserved.
- Use the repository's existing settings, correction, input-safety, history, dictionary, i18n, and UI patterns. Add no speculative compatibility layer or unrelated cleanup.
- Preserve privacy: transcripts, selected text, history content, API keys, clipboard data, and application context must not be logged or sent outside the provider/mode explicitly selected by the user.
- Use Conventional Commits in English, one issue intent per branch. Keep commits local unless publication is explicitly requested.

Reuse / creation plan:
- #7 extends the current provider abstraction and shared correction prompt/streaming path.
- #8 and #9 reuse microphone capture, target capture, overlay state, history, correction transport, and guarded replacement primitives.
- #10 reuses #9 selection/edit primitives and adds a bounded assistant result/action router; arbitrary command or URL execution remains forbidden.
- #11 connects existing `app_category` and `app_scope` fields before adding profile persistence/routing.
- #12 extends current SQLite history and History UI after all voice modes can produce typed history; retained audio must use explicit artifact ownership and cleanup rules.
- #13 extends current dictionary CRUD/hints after #11 activates `app_scope`, and adds source/search/import/candidate workflows without replacing the data model wholesale.
- #14 extends the combined voice-mode settings contracts/UI and shortcut registration transactionally; Windows audio integration requires a safe supported mechanism or a clearly reported platform limitation.
- #15 extends the correction mode/prompt contract and fixtures after #11 context/profile routing exists.

Acceptance criteria:
- Every actionable issue has a dedicated branch, worktree, issue-local task contract, implementation commit(s), and a clean worktree at handoff.
- Each issue-local contract maps every acceptance criterion from its GitHub issue to a reproducible test, build, or explicit review check.
- Existing OpenAI/Gemini correction, Dictate, selected-text translation, localization, history privacy, clipboard/focus/IME guards, and legacy settings migration remain green in every affected branch.
- Maintained tests are added only for changed observable behavior at the cheapest stable seam; all issue-specific focused checks pass.
- Before handoff, each branch passes the narrowest relevant frontend/Rust/Python checks and `git diff --check`; any unrun hardware/manual check is stated rather than inferred.
- Parent roadmap issues #16-#19 receive no duplicate code branch; their child-to-branch mapping is reported in the final handoff.

Open questions:
- Resolved by task structure: tracking issues #16-#19 do not need worktrees because their complete scope is represented by child issues #7-#15.
- Resolved after advisor review: use `#7 -> #8 -> #9 -> #10 -> #12 -> #14`, plus `#11 -> #13` and `#11 -> #15`; only #7 and #11 start directly from the recorded `origin/main` commit.
- May decide per issue after evidence: exact UI composition and internal module boundaries, provided the issue contract and existing repository conventions are preserved.
- Needs user confirmation only if completion requires installing/bundling a model server, enabling an unsafe Windows audio-control mechanism, publishing branches, or merging into `main`.

Context:
- GitHub issue bodies #7-#19 were retrieved from the public REST API on 2026-09-22; #16-#19 explicitly enumerate #7-#15 as their child scope.
- `docs/tasks/open-issues-parallel.md` records the completed #1-#3 workflow and establishes the prior branch-per-issue convention.
- `.worktrees/issue-7-local-llm-correction/docs/tasks/issue-7-local-llm-correction.md` is the existing #7 contract.
- `C:/Users/Koshi/agent-memory/tasks/asr--live-dictation.md` records prior live-dictation verification limits and protected runtime artifacts.

Completion (2026-09-27): All nine actionable child issues are implemented, independently audited, locally committed, and clean in dedicated branches/worktrees. The final local tips are #7 `6198845`, #8 `acadb3c`, #9 `3d0b1a7`, #10 `6411024`, #11 `99436d5`, #12 `1f9c7a6`, #13 `b950c8b`, #14 `da1a58e`, and #15 `f4170fd`. The dependency stack remains #7 -> #8 -> #9 -> #10 -> #12 -> #14, plus #11 -> #13 and #11 -> #15. Parent issues #16-#19 map to those child branches and have no duplicate code worktrees.

Verification by issue is recorded in each issue-local contract. Final #12 checks passed Rust 164/4 ignored and frontend production build; final #14 checks passed Rust 175/4 ignored, Python 53, frontend production build, TypeScript, format/check, and diff checks. All independent review findings were resolved or explicitly documented as manual/backend limitations. No branch has been pushed or merged into `main`.

Manual verification remains for Windows global shortcut registration, microphone selection/level/teardown, audible interaction cues, theme appearance across windows, retained History playback/download and locked-file recovery, cancellation timing, and live ASR/AI provider quality. #14 does not mute or pause other applications: safe Windows ducking requires a communications-stream integration absent from the current CPAL path; the limitation is visible in Settings and documented in the issue contract. Provider locale behavior is disclosed there too: faster-whisper/OpenAI-compatible honor base language only, and VibeVoice currently ignores the locale setting.

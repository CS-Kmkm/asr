Goal: Review the current implementations for GitHub issues #7-#15, publish an evidence-backed comment to every open issue #7-#19, and ensure each actionable issue has an open, correctly scoped PR.

Scope / non-scope:
- Review the existing nine issue branch heads and their complete PR diffs against live issue requirements and issue-local contracts.
- Review defects, contract coverage, branch scope, dependency ancestry, and current CI evidence.
- Reuse existing open PRs #20-#28; create a PR only if an actionable issue lacks one. #16-#19 are roadmap issues and receive aggregate comments, not duplicate implementation PRs.
- No implementation changes, PR merges, issue closures, branch deletion, or history rewrites are authorized by this review request.

Constraints:
- Preserve user changes and the established stacks: #7 -> #8 -> #9 -> #10 -> #12 -> #14; #11 -> #13 and #11 -> #15.
- Distinguish code-confirmed behavior, CI checks, previous local checks, and unverified Windows/provider behavior.
- Publish concrete findings with severity, trigger, impact, and commit-pinned code references. Do not equate passing CI with full feature parity.
- Native independent review is used because the installed Claude CLI remains 2.1.229; the previous authenticated attempt established that the configured Opus model requires 2.1.280 or newer.

Reuse / creation plan:
- Reuse issue-local contracts, current PR descriptions, and the previous remediation checklist; the live issue body is authoritative for requested coverage.
- This file holds the review scope and results. No new implementation branch is needed for a review-only task with existing PRs.

Acceptance criteria:
1. Live open issues and PR metadata are reconciled; local heads match PR heads and dependency ancestry is valid (GitHub tools and git).
2. Every complete branch diff and commit list is scoped to its issue or a justified dependency (commit-split PR scope gate).
3. Independent defect review and main-session contract review are completed for all nine implementations; confirmed findings and material limitations are recorded.
4. Current-head CI evidence is verified. Reuse prior passing local checks for unchanged trees; do not claim new local test runs.
5. A review comment is posted and verified on each open issue #7-#19; nine actionable issues have an open PR with accurate review and validation disclosures.

Issue requirement review lenses (normalized from live GitHub issue bodies retrieved 2026-10-02):
- #7: loopback-only local OpenAI-compatible correction without API keys; shared provider modes, hints, streaming, cancellation and fallback; settings selection and transport regression tests.
- #8: independent voice Translate, capture/ASR/translation/safe insertion, ordered targets and overlay switching, protected content, fallback and cancellation.
- #9: selected text plus separately framed spoken instructions, bounded safe selection replacement, focus/selection/input/IME guards, recoverable fallback and boundary tests.
- #10: selected/no-selection routing, rewrite/draft insertion, answer panel, four fixed-site search actions, separated inputs, Ask history, language behavior, no arbitrary commands or URLs.
- #11: application/category capture, history/dictionary/profile routing, global/scoped abstract style preferences, ON/OFF privacy, manual first-stage profiles permitted; report limitations disclosed.
- #12: filters/deletion/all retention options, retained-audio playback/export/Retry with current settings, history-off and audio cleanup privacy combinations.
- #13: CRUD/search/source filter/CSV, validation and collisions, correction candidates with confirmation, reading/alias/priority/scope recognition hints.
- #14: multiple mode shortcuts and transactional rollback, microphone levels and cues, theme, extensible UI language and regional variant propagation, migration; audio ducking research limitation disclosed.
- #15: opt-in intent-aware reconstruction while preserving conservative behavior, later corrections/repetition/switch precedence, protected facts and uncertainty fixtures.
- #16-#19: aggregate roadmap progress and child PR mapping only; no independent feature changes.

Context:
- Original issue/PR source: https://github.com/CS-Kmkm/asr.
- Prior fix contract: docs/tasks/pr-review-fixes.md. Its September status is stale; current GitHub/local refs determine this review's targets.
- Current issue/PR/head mapping: #7/#20/141c9de, #8/#22/85d933a, #9/#25/06258b6, #10/#26/c17989c, #11/#21/6d90378, #12/#27/8f01bf4, #13/#23/a3e4e59, #14/#28/1549f46, #15/#24/a943362.

Status:
- All five review/publication criteria are complete as of 2026-10-02.
- Criterion 1: nine existing PRs are open at the expected local heads; seven feature-stack ancestry edges pass. Local main contains additional task-documentation commits, so it is not an ancestor of either root feature branch; their live PR base remains the established remote main. No missing actionable-issue PR was found.
- Criterion 2: every branch commit and changed-file inventory maps to the issue or declared dependency. Relevant implementation diffs were inspected. No unrelated intent/file was identified. The independent review was bounded to feature paths and callers, not an exhaustive line-by-line proof of every large test/diff hunk.
- Criterion 3: independent native defect review and main-session requirement review found two Medium defects (below). No additional concrete defect was found for the other seven features; no Blocker/High was identified in the reviewed paths.
- Criterion 4: current-head GitHub CI succeeded for all nine PRs (Windows Rust format/Clippy/library tests, frontend TypeScript, Python worker tests). Prior same-tree local build/check evidence was reused; local test/build commands were not rerun in this review. All nine issue worktrees remain clean and git diff --check passes.
- Criterion 5: exactly one dated review comment was posted and fetched back on each of the thirteen open issues. All nine PR bodies now link the individual review, current-head CI, limitations, and applicable findings; they were fetched back and verified open/unmerged. No new PR was created because all nine already existed. Parent roadmaps have aggregate comments, not implementation PRs.
- No implementation code, branch history, merge state, issue state, or roadmap checkbox was changed. This local review contract/report is uncommitted on the main worktree.
- Remaining implementation work: resolve the two Medium findings in their originating issue branches and propagate to dependents; perform opt-in Windows/provider checks and cross-stack integration before claiming complete parity.

Confirmed findings:
1. Medium, #10 at c17989c, src-tauri/src/commands.rs:1115-1145: Search checks cancellation only before browser launch, then writes History and Completed without a lifecycle check/commit boundary. Cancellation during or after launch can still be followed by those writes. The mechanism is code-confirmed; Windows timing is unverified. Serialize search commitment with cancellation or prevent post-cancellation writes. Inherited at #12 commands.rs:1162-1188 and #14 commands.rs:1372-1397.
2. Medium, #14 at 1549f46, src/pages/SettingsPage.tsx:161-203 and 287-296: voice shortcuts and selected-text translation submit separate settings patches. Swapping their saved chords is rejected against the unchanged old chord even though the combined draft is valid. Save both fields atomically and avoid separately persisting the intermediate blur state.

Publication evidence:
| Issue | PR | Review comment |
| --- | --- | --- |
| #7 | https://github.com/CS-Kmkm/asr/pull/20 | https://github.com/CS-Kmkm/asr/issues/7#issuecomment-5952617581 |
| #8 | https://github.com/CS-Kmkm/asr/pull/22 | https://github.com/CS-Kmkm/asr/issues/8#issuecomment-5952619024 |
| #9 | https://github.com/CS-Kmkm/asr/pull/25 | https://github.com/CS-Kmkm/asr/issues/9#issuecomment-5952620484 |
| #10 | https://github.com/CS-Kmkm/asr/pull/26 | https://github.com/CS-Kmkm/asr/issues/10#issuecomment-5952622407 |
| #11 | https://github.com/CS-Kmkm/asr/pull/21 | https://github.com/CS-Kmkm/asr/issues/11#issuecomment-5952623751 |
| #12 | https://github.com/CS-Kmkm/asr/pull/27 | https://github.com/CS-Kmkm/asr/issues/12#issuecomment-5952624761 |
| #13 | https://github.com/CS-Kmkm/asr/pull/23 | https://github.com/CS-Kmkm/asr/issues/13#issuecomment-5952625675 |
| #14 | https://github.com/CS-Kmkm/asr/pull/28 | https://github.com/CS-Kmkm/asr/issues/14#issuecomment-5952626937 |
| #15 | https://github.com/CS-Kmkm/asr/pull/24 | https://github.com/CS-Kmkm/asr/issues/15#issuecomment-5952628417 |
| #16 | Child PRs above | https://github.com/CS-Kmkm/asr/issues/16#issuecomment-5952650262 |
| #17 | Child PRs above | https://github.com/CS-Kmkm/asr/issues/17#issuecomment-5952646629 |
| #18 | Child PRs above | https://github.com/CS-Kmkm/asr/issues/18#issuecomment-5952647949 |
| #19 | Child PRs above | https://github.com/CS-Kmkm/asr/issues/19#issuecomment-5952649218 |

Material limits:
- Real Windows microphone, global shortcuts, UIA selection, focus/IME/input guards, panel placement, browser launch, audio playback/download, file-lock recovery, cues and themes remain manual checks.
- Provider behavior, local-loopback server interoperability and real Japanese/English/multilingual quality remain unverified.
- #11 is the explicitly permitted manual-profile first stage; it has no automatic learning/report of learned preferences, and app-key/UWP discovery is limited.
- #13 candidates come from AI correction differences and require confirmation/History ON; they do not observe user edits in other applications.
- #14 has no other-app audio ducking; backend locale support is limited. #15 fact checks are syntactic, not a semantic guarantee against invented names or changed relationships.

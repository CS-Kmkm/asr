Goal: Implement GitHub issue #10 as a bounded voice assistant mode whose model may select semantics but can never directly select a side effect.

Scope / non-scope:
- Add Ask shortcut/lifecycle/overlay, selected-text transformations and questions, no-selection answers/drafts, a separate answer panel, four fixed search actions, and typed History.
- Support Japanese/English plus Issue #8's configured translation-language allowlist.
- Never execute commands/shell, accept arbitrary URLs, repair malformed action output, or insert live ASR hypotheses.

Fixed context and delivery design:
- Add `PipelineMode::Ask`, mutually exclusive with Dictate/Translate/Edit and selected-text translation.
- Capture one immutable context before recording: `Selected` reuses Issue #9's selection session; `Caret` captures a full target/caret snapshot and monitor checkpoint; `Unavailable` permits only panel answers/search. Never downgrade unreadable selection into an insertion target.
- Selected rewrite/shorten/expand/tone/translate replaces the original selection; selected summarize/explain/question displays the panel. Caret draft inserts at the original caret; ambiguous no-selection requests display the panel. Answers never mutate the target.
- Unsafe replacement/draft copies the generated result to clipboard, never recaptures/retries, and uses the existing focus/text/selection/input/shortcut/IME/paste guards.

Typed action boundary:
- Use a strict versioned serde enum with `deny_unknown_fields`: rewrite, shorten, expand, change_tone, summarize, explain, translate(target_language), answer, draft, or search(site, query). Supported search sites are only Google, YouTube, Amazon Japan, and GitHub.
- Planning receives only spoken instruction, context kind, and protocol version; it never receives selected text. Rust enforces an explicit context/intent policy table before generation or delivery.
- Generation is a second provider request with an action-specific trusted prompt; selected source and spoken instruction stay separate untrusted fields and output is text only.
- Strictly reject malformed/fenced/unknown/oversized/control-character JSON. Fail closed without mutation/action; never heuristically repair it.
- Search additionally requires the original spoken instruction to explicitly name the parsed site. Query uses spoken input only. Rust builds one fixed HTTPS URL from `(SearchSite, percent_encoded_query)`; no IPC/backend accepts an arbitrary URL. Valid searches open immediately. Missing translation target returns a clarification instead of guessing.

Answer panel:
- Create a separate initially hidden always-on-top `ask-answer` window, not the recording overlay.
- Backend owns versioned state `{ operation_id, payload }`; only the current completion may publish, stale dismiss/update cannot affect newer content, and cancellation does not erase the previous completed answer.
- Show without stealing focus; explicit click enables copy/dismiss. If display fails, retain answer in app state and copy it to clipboard.

History:
- Extend Issue #9 migration with nullable `action_kind` and `search_site`.
- Completed `mode=ask` rows store immutable selected source if any, ASR instruction, generated output (or normalized search query), action kind/site/target language/provider. Do not store cancelled, ASR/provider/planner/policy failures. Respect History-off/retention/audio cleanup.

Acceptance checks:
1. Strict parser/policy-table tests cover every context/intent and reject unknown actions, URLs, commands, shell, extra fields, malformed/fenced/oversized/control data.
2. Prompt tests prove planning excludes selected text and generation keeps source/instruction separate.
3. Fixed search URL tests cover every site and hostile/Unicode query encoding; invalid policy never opens a browser.
4. Lifecycle tests cover exclusion, rapid stop, cancellation/late results before every mutation/panel/search/history boundary.
5. Selected replacement and caret draft route through immutable Issue #9 safety sessions; answer/search paths never call the injector.
6. Panel state tests cover stale completion/dismiss, latest ownership, cancellation, and display fallback.
7. History migration/round-trip/privacy tests cover all Ask outcome fields and legacy rows.
8. Full Rust/frontend checks and `git diff --check` pass. Manual Windows checks cover real mic/provider, selected/caret safety, panel focus/multi-monitor behavior, browser failure, all four searches, and privacy.

Status (2026-09-23):
- Complete in the Issue #9-based worktree; independent security review and both follow-up passes found no remaining issue, and local commits remain pending.
- Added strict versioned Ask plan parsing and context/action policy, with text-only second-stage generation. Planning receives no selected source. Search URLs are built only from fixed site + percent-encoded query and opened by the backend Windows adapter.
- Added Ask lifecycle/session capture, monitored selection/caret clipboard fallbacks, versioned answer-panel get/dismiss ownership, Ask History fields/migration, default `Ctrl+Shift+A`, and the minimal `ask-answer` capability allowlist entry. Translation and fixed-site search now require explicit spoken names, and cancellation cannot introduce a new clipboard mutation.
- Automated checks: `cargo test --lib` (151 passed, 4 ignored), `cargo fmt --all -- --check`, `cargo check --lib`, `pnpm.cmd exec tsc --noEmit`, production frontend build, and `git diff --check` passed. Strict clippy remains blocked by pre-existing unrelated warnings in injection/input-monitor code; Issue #10 additions produce no strict-Clippy diagnostic.
- Manual Windows verification remains required: microphone/provider; original selected/caret target and focus changes; IME/shortcut release; multi-monitor panel focus/copy/dismiss and display fallback; fixed browser failure/all four searches; History-off and retention privacy.

# Progressive settings help — issue #72 follow-up

Goal: Reduce always-visible settings prose while keeping help discoverable by pointer, keyboard and touch.

Scope: Existing shared settings rows, input/shortcut/writing settings, AI correction, models, privacy and credential presentation. Preserve save behavior, validation, drafts and all backend semantics. The prior UI refinement work remains uncommitted; retain it.

Design: Labels and controls form the default view. Nonessential short descriptions move to a help marker that opens on hover/focus/click, remains hoverable and closes with Escape. Long background explanations move into collapsed section help. Keep concise inline summaries for deletion, external sending and material experimental/AI limitations. Do not put form controls in tooltips or truncate warnings with ellipsis.

Reuse / creation: Extend SettingRow and reuse existing disclosure patterns. Add HelpHint in the shared UI module and focused accessibility/interaction tests. No dependencies, new settings keys or backend changes.

Acceptance criteria:

1. Default settings rows no longer show ordinary explanatory paragraphs; short help opens on hover, keyboard focus and click, and Escape dismisses it. Test shared controls.
2. Tooltips stay within the viewport at native widths 900 and 1180 and remain readable/hoverable; scrolling and leaving a category dismiss them. Inspect native UI and test state cleanup.
3. Long section prose is collapsed and duplicated/inapplicable notes are reduced. Destructive/send/experimental summaries remain concise and visible. Review source and focused tests in English/Japanese.
4. Existing settings persistence, IME handling, draft preservation and microphone cleanup tests still pass. Run pnpm test and pnpm run build, plus independent audit.

Context: src/components/ui.tsx, AiCorrectionSettings.tsx, ApiKeySettings.tsx, RecentAppPicker.tsx, src/pages/SettingsPage.tsx, ModelsPage.tsx, PrivacyPage.tsx, src/styles.css and src/test/*. Prior verification: docs/tasks/ui-refinement.md.

Verification:

- Criteria 1 and 3: implemented and tested. Routine detail text is available from a question-mark help marker on hover, focus or click; Escape dismisses it. Associated screen-reader descriptions remain on native fields and component controls, and CommitField preserves its validation-error description. Long section prose is collapsed. Inline summaries remain for retention/deletion, Edit/Ask/Translate sending, intent-aware limitations and experimental insertion.
- Criterion 2: native Tauri inspection covered Japanese/Dark at 1180×760 and 900px width, plus English/Light at 900px. Checked default Input and Writing layouts, click/focus help, Escape dismissal, upward placement near the bottom and leftward clamping near the right edge. The English language-add row fits without wrapping its action button. Automated tests cover hoverability and viewport/outside dismissal; native automation did not provide a hover-only pointer movement API. This is representative inspection, not an exhaustive matrix of every state.
- Criterion 4: `pnpm test` passed 142 tests in 17 files; `pnpm run build` and `git diff --check` passed. Logs: `.ai/test-results/ui-help-tests-reviewed.log` and `ui-help-build-final.log`. The independent native Codex reviewer identified missing description forwarding on component controls and shortcut send summaries; both were fixed, regression-tested and rechecked with no confirmed nearby finding remaining.
  The test run preceded removal of three unused newly introduced translation pairs; no behavior changed after that run. The final build and diff check include that catalogue cleanup.

Status: Implementation, automated checks, representative visual inspection and independent review completed. The prior verification summary was published to CS-Kmkm/asr issue #72 with the user's explicit approval. UI language/theme were restored to Japanese/System, and the native window to standard size. No recognition/provider requests, consent changes, recording, model download or data deletion were initiated for visual checks. No commit or push has been performed.

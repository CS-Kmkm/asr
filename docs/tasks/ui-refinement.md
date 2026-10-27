# UI refinement — issue #72

Goal: Make everyday actions and related settings understandable through layout and hierarchy, with less explanatory text.

Scope: React navigation, dashboard, settings, history, dictionary, setup, model presentation, shared controls, translations and CSS. Preserve backend behavior, settings persistence and data retention semantics. No commits or pushes are authorized.

Constraints: Keep external-send consent, destructive-action explanations and experimental limitations visible at the relevant action. Preserve keyboard access, draft edits and microphone-test cleanup. Use existing pnpm tooling and React/Tauri components; add no dependencies.

Reuse / creation plan: Extend existing page components and SettingRow. Add this contract and focused interaction tests. Keep technical details in disclosure controls; avoid replacing labels with unlabeled icons.

Layout: Primary navigation is Home, History, Dictionary; Settings opens grouped secondary navigation. Home orders recording, latest result, then compact model/data-flow status. Settings separates input, shortcuts, writing/translation, models/connections, privacy and general options. History groups management and reprocessing; dictionary opens the entry editor only on demand. Setup uses ordered microphone, model, test groups.

Acceptance criteria:

1. Home exposes recording state/action, explicit result copy and preparation/recovery navigation. Verify interaction tests and visual inspection.
2. Settings categories group related fields without changing save/validation behavior or losing drafts on category switches. Verify existing persistence tests plus category interaction tests.
3. Self-evident help is removed, SettingRow supports absent help without broken ARIA references, important warnings remain. Verify shared-control tests and source review.
4. History, dictionary and setup follow the same hierarchy; existing search/retry/import and test workflows remain accessible. Verify focused and existing tests.
5. English/Japanese, light/dark at 1180×760 and 900px have no horizontal overflow; controls remain keyboard accessible. Verify browser UI inspection and responsive styles.
6. pnpm test and pnpm build pass; one independent audit covers defects, maintainability and contract compliance. Record actual results and remaining limits.

Open questions: Routine grouping and spacing may be decided locally based on this layout. No user confirmation required for reversible UI changes in this scope.

Context: src/App.tsx, src/pages/*, src/components/ui.tsx, src/i18n.tsx, src/styles.css, src/test/*, issue https://github.com/CS-Kmkm/asr/issues/72. Baseline worktree is clean.

Status:

- Criteria 1–4: implemented. Home has explicit copy/history/model/privacy/shortcut actions. Settings retains drafts between categories and model/privacy views, and stops microphone tests on category exit. History reprocessing choices are owned by each entry. Dictionary editing and CSV import open on demand. Setup uses three ordered groups. Important privacy summaries remain visible with full explanations in Details.
- Criterion 5: manual visual verification completed in the native Tauri development app after the user authorized `local-voice-input.exe` and reloaded VS Code. General settings were sampled in Japanese/English and light/dark at standard and minimum widths. Additional stress views included Home, populated History with management/reprocessing expanded, Dictionary editor, Input, Shortcuts, Writing/translation, Models/API-key rows, Privacy and Setup. Screenshots were inspected in the conversation; no programmatic DOM overflow measurements were taken. No visible horizontal scrollbar remained in the checked long-content views after the fix. This is representative visual coverage, not an exhaustive state-by-state matrix or an ASR/insertion end-to-end test.
- Criterion 6: `pnpm test` passed 136 tests in 17 files; `pnpm run build` and `git diff --check` passed after the visual follow-up. Logs: `.ai/test-results/ui-visual-followup-tests.log` and `ui-visual-followup-build.log`. Claude audit transmission was rejected by automatic approval review; the AGENTS.md native Codex audit fallback was completed. Initial findings, the Japanese-encoding finding and the incremental visual changes were independently reviewed; no confirmed finding remains.

Independent review resolutions:

- Keep model configuration and API-key panels mounted and hidden within Settings so unsaved drafts survive category changes; covered by a regression test.
- Label the home recovery navigation Configure model rather than Retry.
- Keep the limited scope of intent-aware checks, including unchecked names, visible beside the selection; retain full details in disclosure.
- Refocus the dictionary editor when switching edited entries, covered by a regression test.
- Record actual tests/build results and the visual verification scope and limits here. Centralize category navigation and heading labels in one descriptor list to address the maintainability finding.
- Restore four Japanese summaries that a shell encoding conversion had replaced with question marks. The regression test checks readable Japanese safety summaries outside closed disclosures; the reviewer verified the restored UTF-8 strings and absence of repeated question marks.

Visual follow-up:

- At the native minimum client width of 900px, a 900px body minimum combined with a vertical scrollbar created horizontal overflow. Remove the CSS body minimum and retain the native minimum in Tauri. Confirmed manually in long-content views without hiding overflow.
- Keep the sidebar sticky with its own vertical overflow so categories remain reachable while the main content scrolls. Reset document scroll on page/category navigation; the existing category/draft test now checks that behavior too.
- Use a shorter Shortcuts navigation label while preserving the full page heading. Add themed base action-button styling and danger coloring; scope API-key row spacing to its own component. The independent reviewer checked overrides for recording-overlay and Ask buttons.
- Verified native client widths of 1180px and 900px with 760px client height through the initial Tauri window and native resizing. No model download, recording, insertion, external consent or data deletion was initiated. UI language and appearance were restored to Japanese/System after testing.

Status: implementation, automated checks, representative native visual verification and independent review are complete. Issue #72 remains open for integration because the changes are local and uncommitted. No commit or push has been performed.

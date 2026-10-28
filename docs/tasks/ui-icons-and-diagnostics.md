# Icon controls, help and diagnostics follow-up

Goal: Replace repetitive navigation/action text with recognizable icons, unify explanatory help under question marks, use discrete sliders for ordered settings and repair diagnostic layout.

Follow-up scope: Extend common add/save/edit/delete/refresh/copy controls throughout settings, dictionary, recent-app picker, API keys, setup, custom-model registration and results to the existing IconButton. Keep recording, consent, model preparation and text-processing actions readable. Preserve callback arguments, submit types, disabled conditions and deletion confirmations. Check existing behavior tests, build and independent incremental audit; native visual acceptance remains dependent on Computer Use availability.

Follow-up validation: Full 146 tests passed after adapting the setup Copy result test to query the accessible button name. The final custom-model modal edit additionally passed all 46 app tests. Final TypeScript/Vite build and diff check passed. Logs: .ai/test-results/ui-action-icons-tests.log, ui-action-icons-model.log and ui-action-icons-build.log. Independent review identified a light-theme primary-icon hover contrast regression; generic icon hover now excludes primary buttons, preserving their existing foreground/background pairing. The reviewer verified this fix and reported no remaining confirmed actionable finding. Native visual verification remains pending.

Shortcut recording follow-up: Replaced Record shortcut text with the shared keyboard SVG and recording-state Cancel with close SVG. Localized aria-label/title and the visible recording prompt remain. Preserve input focus on a recording-state mouse press so cancellation cannot blur/finish then restart capture. Focused 15 settings-control tests and TypeScript/Vite build passed; added a capture-release regression check. Logs: .ai/test-results/ui-shortcut-icon-tests.log and ui-shortcut-icon-build.log. Independent review found no confirmed defect; native visual verification still pending.

Shortcut placement follow-up: Moved each mode's existing add button from a solitary bottom row to the top-right of its shortcut group beside the hold control. Grid columns align entry fields, record controls and delete buttons; warning lines span the group. Callback/disabled/save semantics unchanged. Related 61 tests, TypeScript/Vite build and diff check passed; independent review found no confirmed regression. Native visual verification remains pending because the connection still reports missing pipe. Logs: .ai/test-results/ui-shortcut-layout-tests.log and ui-shortcut-layout-build.log.

Scope: App navigation, history action controls, shared icons/help, settings noise suppression and reasoning effort, static explanatory disclosures, diagnostic cards/actions/facts/tables. Preserve all existing settings/storage, retry, consent and diagnostic-copy semantics. Keep arbitrary language/model/provider names readable. Prior UI changes remain uncommitted and must be retained.

Design: Use a consistent code-native SVG icon set with accessible labels and hover names. Keep destructive/send summaries visible. Question-mark help supports keyboard focus/click/Escape, and long help can scroll without closing. Structural panels that contain controls remain disclosures. Sliders map exact ordinal values and commit at interaction completion. Diagnostics tables stay within individually scrollable containers with headers/captions and keyboard access.

Reuse / creation: Add a small shared SVG icon component; extend existing UI help and field components. Reuse existing tests and add meaningful action mapping, ordinal commit, long-help accessibility and diagnostic long-content tests. No dependencies or backend changes.

Acceptance criteria:

1. Navigation and common history actions use icons with preserved accessible labels, selected state and keyboard behavior. Validate interaction tests and native screens.
2. Static settings Details/About explanations use question-mark help; long text is scrollable/readable and dismissible. Validate hover/focus/click/Escape/scroll tests and native positions.
3. Noise suppression and reasoning effort use discrete sliders with current value and exact existing stored values; no per-pointer-move saving. Verify pointer/keyboard and external-setting updates.
4. Diagnostics has separated sections, spaced actions, wrapping facts and bounded table scrolling; no page-wide overflow at 900/1180 widths. Preserve report-copy exclusions and explicit worker probing. Verify existing/focused tests and native populated tables.
5. Full pnpm tests, build and diff checks pass; obtain independent audit of this incremental change.

Context: src/App.tsx, src/components/ui.tsx, CommitField.tsx, AiCorrectionSettings.tsx, src/pages/SettingsPage.tsx, HistoryPage.tsx, DiagnosticsPage.tsx, src/styles.css. Earlier contracts describe the preserved UI baseline.

Implementation: Icon rail, accessible history filter/action buttons, unified question-mark help, discrete noise/reasoning sliders and diagnostic sections/scroll containers are implemented. Structural GPU controls and important inline summaries remain visible. Existing backend and copy exclusions are preserved.

Validation: 146 frontend tests across 17 files passed, including exact ordinal mapping/deferred commits/external updates, keyboard focus and internal help scrolling, history icon filtering and bounded diagnostic table structure. TypeScript/Vite build and git diff --check passed. Logs: .ai/test-results/ui-icons-tests.log and ui-icons-build.log.

Independent audit: The existing read-only native reviewer found no confirmed actionable defect in this increment. It checked stored enum mapping, deferred saves, icon accessibility/actions, help focus/scroll/Escape, diagnostics copy exclusions and explicit probing. It independently identified native visual acceptance as unverified.

Status: Implementation and automated/review checks complete; native post-change visual validation at 900/1180 widths is pending. Computer Use reports native pipe unavailable (os error 2), including the documented retry and session reset/reinitialization. Do not claim screenshot verification or completed visual acceptance. The earlier native inspection established crowded diagnostics controls and unstyled wide tables. No commit/push authorized.

# UI polish pass

Goal: Resolve remaining UI inconsistencies found in a source review after the #72 passes.

Scope: History reprocessing language names, Home status duplication, privacy consent toggle names, notification/history/empty-state control styling, dictionary entry/candidate presentation, scoped profile field captions, a Setup return path from Models and switch semantics for toggles. Icon-only confirm actions stay as they are (user decision 2026-10-28). No backend, settings schema or persistence changes; no new dependencies.

Acceptance:

1. History reprocessing translation targets show localized language names while submitting the same codes.
2. Home shows the current phase once; other pages keep the header status.
3. Cloud consent toggles use provider display names in their accessible names.
4. Notification copy/dismiss use shared icon controls; retry cancel and Load more use themed buttons; empty states use the shared icon set.
5. Dictionary rows omit default priority and manual-source noise; candidates show original → preferred once. Existing callbacks and contextual names are unchanged.
6. Scoped profile fields have visible captions; scope comes first.
7. Leaving Setup for Models offers a return to Setup that disappears after navigating elsewhere.
8. Toggles expose role="switch" with aria-checked.
9. pnpm test, pnpm run build and git diff --check pass; one independent audit. Native visual verification is out of reach in this session (browser extension blocked the dev server); do not claim it.

Implementation: Shared translationLanguageRegistry supplies History reprocessing names. Home omits the header status pill. Consent switches are named "OpenAI/Gemini: Cloud provider consent". Notification copy/dismiss use IconButton; retry Cancel and Load more are themed; Empty takes an icon. Dictionary rows show auto source, non-zero priority and scope as badges; candidates render original → preferred once. Scoped profile fields have captions contained in their accessible names, scope first; Remove stays last in DOM/tab order and is placed top-right by CSS. Setup → Models shows Back to setup until another page or category is opened. Toggle is role="switch" with aria-checked; tests query switches.

Validation: 160 tests across 17 files, the 39 node script tests (updated for the #72 icon controls after CI caught them), TypeScript/Vite build and git diff --check passed. Logs .ai/test-results/ui-polish-tests.log and ui-polish-build.log. The Codex audit route failed (gpt-6-sol not supported for this ChatGPT account); the disclosed fallback, one independent Opus 5.5 reviewer, found caption/name mismatch, mid-row Remove tab order and two overridden CSS margins. All were fixed with a regression test for captions/order; the reviewer confirmed the fixes and reported no new defect. Native visual verification was not performed: the Chrome extension was blocked from the Vite dev server.

Status: implementation, automated checks and independent review complete; native visual verification pending.

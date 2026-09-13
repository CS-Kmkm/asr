Goal: Resolve the remaining Issue #2 feedback by making notification and highlight colors match their semantic meaning within the light blue base palette.

Scope / non-scope:
- Classify backend status notices as info, success, warning, or error in the frontend.
- Use blue for neutral information/highlights, green for success, amber for recoverable warnings, and red for errors.
- Do not change application behavior, backend event payloads, layout, or unrelated component styling.
- Preserve existing uncommitted changes in the primary worktree.

Constraints:
- Base the fix on current `origin/main` (`cb2e974`).
- Reuse the existing status `kind`, notice component, and CSS custom properties.
- Keep text contrast suitable for the light surfaces.

Reuse / creation plan:
- Extend the `Notice` severity contract and status-kind mapping in `src/App.tsx`.
- Extend the existing semantic palette overrides in `src/styles.css`.
- Add no dependency or test framework.

Acceptance criteria:
- Backend statuses render as info, success, warning, or error according to their existing `kind`; verify by code review of every emitted kind.
- Neutral notices/highlights use the blue base palette, warnings use amber, successes use green, and errors use red; verify by CSS review and contrast calculation.
- `pnpm exec tsc --noEmit` and `pnpm run build` pass.
- Existing Rust and Python suites pass before Issues #1 and #3 are closed as implemented.
- The verified fix is present on `origin/main`, and Issues #1, #2, and #3 are closed with resolution comments.

Open questions:
- None. The Issue #2 follow-up comment explicitly requests consistent warning/highlight color usage; semantic status colors preserve meaning while aligning neutral highlights to the base blue.

Context:
- Issue #2 follow-up comment dated 2026-09-12 reports inconsistent warning and highlight colors.
- PRs #4, #5, and #6 are merged into `origin/main`; Issues #1, #2, and #3 remain open.
- `src/App.tsx` currently maps every backend status event to `info`, while `.notice` currently uses the success palette by default.

Completion status (2026-09-12):
- Implemented semantic backend-status classification and explicit success notices in commit `16b9100` (`Refs #2`).
- Neutral/info notices now use the base blue palette; success, warning, and error notices use their green, amber, and red semantic palettes.
- Checked every currently emitted backend status kind against the classification. Unknown future kinds safely remain informational.
- Foreground/background contrast ratios are info 6.79:1, success 5.76:1, warning 6.09:1, and error 8.57:1.
- Verification passed: `pnpm exec tsc --noEmit`, `pnpm run build`, 98 Rust library tests, and 57 Python worker tests. Python emitted one dependency deprecation warning.
- Remaining external steps: push the verified commits to `origin/main`, confirm the remote head, comment on and close Issues #1, #2, and #3, then confirm their remote state.
